# weave v0.3 roadmap — beyond the daemon

A forward roadmap that picks up where [`ROADMAP-v0.2.md`](./ROADMAP-v0.2.md)
leaves off. v0.2 lands the workspace split and the optional presence daemon; v0.3
is about turning weave from a *mailbox with push* into a **turn-driving control
plane** for a multi-agent mesh, plus the reach (remote, cross-repo, more
terminals) the competitive sweeps flagged as the highest-leverage gaps.

Everything here is **additive and gated** under the same constraints v0.2 set:

- The default `cargo build` stays a **single static binary, no tokio, no daemon**.
  New runtime surfaces (HTTP MCP, daemon-backed wake) live behind features and
  always degrade to the v0.1 hook-drain path.
- The `Store` trait stays **sync** and **object-safe**; tokio stays confined to
  `LibsqlStore` (and, new in v0.3, to the optional HTTP transport). Backends stay
  **mutually exclusive** (`sqlite` XOR `libsql`).
- Every item keeps `cargo test` green for **both** feature columns, and every new
  column is a guarded migration in the `migrate()` mould (the `in_reply_to`
  pattern in `store.rs`).

The v0.2 roadmap already sketches a "v0.3 tail" (nullable `deliver_at`/`kind`,
libSQL-replica presence, iTerm2). This document promotes that tail to a real plan
and adds the structural wins the sweeps surfaced.

---

## 1. No-poll stop-boundary wake

**Why (sourced positioning).** This is the headline differentiator from the
research sweep (see ROADMAP-v0.2 §"Positioning", item 1): *"a blocking
`Stop`/`SubagentStop` hook that queries weave's local store and returns
`additionalContext` so a peer's message drives the next turn with no in-agent poll
loop."* The whole category weave competes with — brittle `tmux send-keys` rigs and
MCP brokers that a session only sees when it *chooses* to call `inbox` — is
**poll-based**. Keystroke injection (weave's own §4 push model) is the live path,
but it only works when the recipient is in a mux *and* idle at a prompt. The
stop-boundary wake is the path that works **everywhere**, including headless and
non-mux sessions, by hooking the one moment the agent is guaranteed to yield: when
it stops.

The seam already exists and is deliberately under-used. In `main.rs::handle_hook`,
the `"stop"` arm today only **peeks** (mark_read=false) because, in its words,
*"Claude Code does NOT add Stop-hook stdout to the model context on a normal
exit."* That comment is the entire opportunity: Claude Code *does* honour a
**structured** Stop-hook response — a JSON object on stdout with
`{"decision":"block","reason":"…"}` (and the `SubagentStop` equivalent) re-prompts
the agent with `reason` as the next turn's input instead of letting it idle. So
weave can convert "you have unread mail" into "here is the mail, keep going"
**without any in-agent poll loop and without keystroke injection**.

**Additive sketch.**
- New hook event variant `weave hook stop --wake` (and `subagent-stop`), or a
  config/env gate `WEAVE_STOP_WAKE=1`, so the default `stop` behaviour
  (peek-only, byte-identical to v0.1) is untouched for anyone who doesn't opt in.
- In the gated path, after the existing `store.inbox(&me, false, mark_read, 50)`
  drain, if `rows` is non-empty emit a JSON object to stdout:
  `{"decision":"block","reason": <rendered unread messages + "reply via weave_reply / weave_send">}`
  and **mark read** (the wake *is* the delivery, exactly as the `prompt` arm
  already argues). Empty inbox ⇒ emit nothing / exit 0 ⇒ the agent stops normally.
- Reuse the existing render loop (`#{id} from {sender} ({subject}): {body}`) so
  there's one delivery format. Cap the woken payload with the existing `MAX_BODY`
  / a new turn-budget so a flood can't wedge a session in a wake loop.
- A small **wake-loop guard**: track a per-session "woke at message id N" high-water
  mark (a new `peers` column or a tiny `wake_state` row) so two agents pinging each
  other can't spin forever — only genuinely *new* unread since the last wake blocks
  the stop.

**Rough effort.** Small–medium. ~1 new hook arm, one structured-stdout path, one
guard column (guarded migration), and tests asserting the exact JSON shape and the
"empty inbox ⇒ clean exit" / "loop guard" invariants. No new dependency, no daemon.
The risk is entirely in matching the agent's Stop-hook JSON contract precisely; it
should be a documented, tested constant, not guesswork.

---

## 2. `weave_ask` / `weave_ack` as an MCP request–reply task

**Why (sourced positioning).** Today every weave tool is **fire-and-forget**:
`weave_send` returns a message id and whether a nudge landed, but the sender has no
first-class way to *wait for and consume an answer*. Multi-agent orchestration —
the use case the competitive sweep frames weave against — is overwhelmingly
**request/reply** ("ask the build agent to run tests, block on the result"). The
sweep's emphasis on driving the *next turn* (item 1) and on read receipts/idempotency
(below) all point at the same missing primitive: a correlated ask→ack. Without it,
every consumer hand-rolls correlation on top of `weave_send` + polling `weave_inbox`,
which is exactly the poll loop weave is trying to delete.

The pieces already exist. `weave_reply` + the `in_reply_to` column + the recursive
`thread()` CTE already give weave **correlation**; `receipts()` already gives
**"has it been seen."** `weave_ask`/`weave_ack` is the thin, ergonomic task layer
that composes them.

**Additive sketch.**
- `weave_ask` = `weave_send` that returns the new message id **and** an explicit
  contract: "the answer will arrive as a `weave_reply` whose `in_reply_to` is this
  id." Optionally a `kind:"ask"` tag (reuse the v0.2-tail `kind` column) so asks are
  filterable.
- `weave_ack` = sugar over `weave_reply` that additionally stamps a terminal state
  on the parent (a nullable `answered_at` column, guarded migration) so an ask can
  be queried as open/answered without scanning the thread.
- A read-only `weave_await(ask_id)` MCP tool that returns the reply if present,
  else "still open" — the **non-blocking** building block the stop-wake (§1) turns
  into a *blocking* experience: an asker that stops with an open ask gets woken the
  instant the ack lands. Ask+wake together = synchronous-feeling RPC with zero
  polling.
- Pairs naturally with idempotency keys (§3): an `ask` carries one, so a retried
  ask doesn't double-post.

**Rough effort.** Medium. Mostly MCP-layer glue (`tools()` entries +
`call_tool` arms in `mcp.rs`) over existing store primitives, plus one nullable
`answered_at` column and the `weave_await` query. The blocking flavour is *free*
once §1 exists. Keep `weave_ask`/`weave_ack` strictly decomposable into
send/reply/receipts so the data model gains nothing it can't already express — the
value is the named contract, not new storage.

---

## 3. Read receipts, idempotency keys, and trace IDs

**Why (sourced positioning).** The sweep called out **delivery semantics** as a
maturity gap relative to message-bus tooling weave gets compared to. weave already
has **read receipts** (`receipts()` over the `reads` table, exposed as
`weave_receipts`) — so half of this is done and the roadmap's job is to *finish the
trio*:
- **Idempotency keys** — the no-daemon push model and the planned retry paths
  (`run_with_one_retry`, the §1 wake guard) all create at-least-once delivery
  pressure. An optional client-supplied idempotency key makes `weave_send`/`weave_ask`
  **safe to retry** without duplicate mailbox rows — the standard guard any
  at-least-once system needs.
- **Trace IDs** — once agents chain (A asks B, B asks C), an operator needs to
  follow one logical request across threads and sessions. A propagated trace id is
  the difference between a debuggable mesh and N opaque inboxes. This directly
  serves the "observers/dashboards" angle the sweep raised (ROADMAP-v0.2 §Positioning
  item 3, the read-side stream).

**Additive sketch.**
- Two nullable columns on `messages`: `idem_key TEXT` and `trace_id TEXT`, each a
  guarded `ALTER TABLE ADD COLUMN` exactly like `in_reply_to` (defaults NULL ⇒ every
  existing row and every non-opted caller is unchanged).
- A **partial unique index** on `(sender, idem_key)` where `idem_key IS NOT NULL`,
  so a retried send with the same key is a no-op that returns the *original* id
  (the store does an `INSERT … ON CONFLICT DO NOTHING` then re-selects). Bodies are
  already capped (`MAX_BODY`), so this adds no new unbounded surface.
- `trace_id` auto-propagates: `weave_reply` inherits its parent's `trace_id` (the
  same place it already inherits the subject), and a fresh `weave_send`/`weave_ask`
  mints one when absent. Surface it in `weave_thread` / `weave_history` output and
  the `--json` modes so a chain is greppable end-to-end.
- `weave_receipts` is extended to report idempotency collapses ("this id absorbed N
  duplicate sends") for operability.

**Rough effort.** Small–medium per piece; the trio shares one migration. Receipts
are already shipped, so the new work is two columns, one partial index, the
inherit-on-reply wiring, and `--json`/render plumbing. No protocol change above the
backend; libSQL parity is a column copy. The only subtlety is the conflict-and-reselect
path on `send` — one focused test (same key twice ⇒ one row, same id) pins it.

---

## 4. Streamable-HTTP MCP transport for remote / cross-machine

**Why (sourced positioning).** weave's MCP server is **stdio-only**
(`mcp.rs::run` over `stdin`/`stdout`), which means a weave tool can only be called
by an agent **on the same host that spawned it**. The MCP ecosystem has standardised
on a **streamable-HTTP** transport for exactly the remote/multi-client case, and the
sweep flagged cross-machine reach as weave's clearest reach gap — today cross-machine
is "copy the libSQL-compatible file around," not "talk to a running weave." A
streamable-HTTP MCP endpoint lets agents on *other machines* (or other containers)
register, send, and `weave_inbox` against one shared weave, turning the mesh from
single-host to fleet — without touching the local stdio path that the vast majority
of sessions use.

This composes with the libSQL backend weave already ships: `LibsqlStore` gives the
**shared state** across machines; an HTTP MCP front-end gives the **shared control
plane** over it. Note the honest boundary from ARCHITECTURE §8 — cross-machine
*injection* stays out of scope (you can't `tmux send-keys` into another box's pane),
but cross-machine **messaging, asks, receipts, and stop-wake** all work, because
those flow through the store, not the injector.

**Additive sketch.**
- A new `weave mcp --http <addr>` mode behind an `http` feature, reusing the exact
  same `call_tool` / `tools()` dispatch — the JSON-RPC *handlers* are transport-
  agnostic already; only `run()`'s framing loop is stdio-specific. Factor the
  per-request `handle()` out (the v0.2 split already makes `weave-mcp` a library) and
  feed it from an HTTP request body instead of a line.
- Streamable-HTTP framing: a `POST` endpoint that accepts a JSON-RPC request and
  responds either with a single JSON object or an SSE stream for long-running /
  server-push results (this is also the natural home for the read-side SSE the sweep
  asked for, and for delivering an ask's ack as a server event).
- tokio is **already an optional dep** (pulled by `libsql`); the `http` feature
  shares that runtime rather than adding a second one. Bind `127.0.0.1` by default;
  any non-loopback bind **must** be gated behind explicit auth (see below) — the
  current threat model (ARCHITECTURE §7) is *local-trust*, and HTTP is the first
  time that assumption can leak off-box.
- **Identity becomes load-bearing.** §7 of ARCHITECTURE notes identity is advisory
  (free strings, no auth) — fine for a local single-user mesh, **not** fine over a
  socket. So the HTTP transport ships with a bearer-token gate (reuse the
  `libsql_auth_token` config convention) as a hard prerequisite, not an afterthought.

**Rough effort.** Medium–large, and the largest new surface in v0.3. The handler
refactor is cheap (the dispatch is already pure); the cost is a real HTTP/SSE
transport, the auth gate, and a threat-model revision for the off-box case. Strictly
feature-gated so the default binary stays daemon-free and tokio-free. Sequence it
**after** §3 (idempotency) — a networked transport without retry-safety is a
duplicate-message generator.

---

## 5. Shared-repo reservation leases

**Why (sourced positioning).** The competitive sweeps frame weave against
**multi-agent dev** setups where several sessions act on the **same working tree**
(the canonical "swarm of agents on one repo" pattern). The failure mode there isn't
messaging — weave solves that — it's **collision**: two agents editing the same file
or running the same migration. weave already has the two ingredients to solve it that
nobody else on this box combines: a **shared store** (the mailbox DB / libSQL) and a
**peer registry with presence** (`peers`, `is_online`, the v0.2 daemon heartbeat). A
lightweight **reservation lease** ("I'm holding `crates/foo/` for the next 10
minutes") turns weave from a comms bus into a **coordination** layer — a genuinely
differentiated capability, not a me-too feature.

This is deliberately *advisory*, matching weave's existing trust model (ARCHITECTURE
§7: "identity is advisory"). It's a cooperation protocol between friendly agents, not
a mandatory lock — the same philosophy as the rest of the tool.

**Additive sketch.**
- A new guarded table `leases(resource TEXT, holder TEXT, acquired INTEGER,
  expires INTEGER, note TEXT, PRIMARY KEY(resource))` — same idempotent
  `CREATE TABLE IF NOT EXISTS` + `migrate()` discipline as `peers`.
- Three MCP tools + CLI verbs: `weave_reserve {resource, ttl, note}`,
  `weave_release {resource}`, `weave_leases` (list active, with holder + presence +
  expiry). Acquisition is an `INSERT … ON CONFLICT` that **succeeds only if the
  existing lease has expired** (`expires < now()`), so a stale lease from a crashed
  session self-heals on TTL — no daemon required, but the v0.2 presence daemon, when
  present, can proactively release a dead holder's leases via the same lifecycle
  eviction it already does for panes.
- On a failed acquisition, return the current holder so the asker can `weave_ask`
  them ("you're holding `migrations/`, can I take it?") — leases and the ask/ack
  primitive (§2) compose into a negotiation, not a hard failure.
- TTL is mandatory and capped (mirror `clamp_limit`'s "untrusted bound" stance) so a
  forgotten reservation always expires.

**Rough effort.** Small–medium. One table, three thin tools over a single
conflict-aware upsert, and a TTL-expiry test. No new dependency. The presence-daemon
eviction tie-in is optional polish that reuses v0.2 machinery. The design risk is
purely product (what's the right resource granularity — path? glob? freeform tag?);
ship it freeform (`resource` is an opaque string) and let usage decide.

---

## 6. iTerm2 injector backend

**Why (sourced positioning).** The v0.2 tail already names iTerm2 as the next
terminal backend, and the sweep's framing of weave as the **native multi-mux
injector** (vs. tmux-only Python rigs) makes macOS coverage the obvious reach gap:
iTerm2 is the dominant macOS terminal for developers, and a weave that can push into
it natively closes the "works on my Linux box, not my Mac" hole. weave's injector is
explicitly designed to grow backends — `inject.rs` is a pure per-mux command table
(`commands_for`) plus a detection probe (`detect_target`) plus an id validator
(`id_valid`), each already unit-tested per backend. Adding iTerm2 is filling in those
three pure functions for one more `Mux` variant; no architecture changes.

**Additive sketch.**
- New `Mux::Iterm2` variant. Detection: iTerm2 exposes session identity via
  `ITERM_SESSION_ID` (and the AppleScript/`it2` tooling for addressing a specific
  session), so `detect_target()` gains one more `nonempty_env` arm in its existing
  precedence chain.
- `commands_for(Iterm2, …)`: iTerm2 has no `send-keys`-style CLI like tmux, so the
  injection idiom is its scripting bridge — drive `osascript`/AppleScript (or the
  `it2` Python-API helper if present) to `write text` into the addressed session.
  This **must** preserve weave's paste-safety contract: type the literal body as one
  argument (no shell string building — ARCHITECTURE §7's "no shell, ever"), then a
  separate submission step, exactly as the other five backends do. The bracketed-paste
  concern (§3.2 of ARCHITECTURE) applies; mirror wezterm's `--no-paste`-style "type
  then submit" split.
- `id_valid(Iterm2, …)`: `ITERM_SESSION_ID` has a known shape
  (`w<N>t<N>p<N>:<UUID>`); add a validator arm in the `id_valid` style so a poisoned
  registration can't smuggle AppleScript — the higher bar here because the bridge is
  a scripting host, not a `--`-guarded argv.
- A `liveness_probe(Iterm2, …)` (list sessions / window tree) so the §3 opportunistic
  pre-check works for iTerm2 too; fail-open like the rest.

**Rough effort.** Medium. The pure functions are small, but iTerm2 is the first
backend whose submission goes through a **scripting host** rather than a direct
keystroke CLI, so the paste-safe + no-shell + id-validation work is more delicate
than the existing five, and it can only be truly validated on macOS hardware (the
other backends are construction-validated via argv unit tests; iTerm2 needs an
on-box end-to-end check, like the zellij validation v0.2 still tracks). Land the pure
`commands_for`/`id_valid`/`liveness_probe` arms with unit tests first; gate "verified
live" behind real-hardware confirmation.

---

## Sequencing & dependencies

| # | Item | Depends on | Default-build impact | Effort |
|---|------|-----------|----------------------|--------|
| 1 | Stop-boundary wake | v0.1 hooks (have it) | none (opt-in gate) | S–M |
| 2 | `weave_ask`/`weave_ack` | reply+thread+receipts (have), §1 for blocking | none | M |
| 3 | Receipts / idempotency / trace | reads table (have) | none (nullable cols) | S–M |
| 4 | Streamable-HTTP MCP | v0.2 `weave-mcp` lib split, §3 for retry-safety | feature-gated | M–L |
| 5 | Reservation leases | store + presence (have/v0.2) | none (new table) | S–M |
| 6 | iTerm2 backend | `inject.rs` seams (have) | none | M |

Recommended order: **1 → 3 → 2 → 5 → 6 → 4.** The stop-wake (1) is the flagship
differentiator and unlocks the *blocking* flavour of asks; the delivery-semantics
trio (3) makes everything else retry-safe and must precede any networked transport;
asks (2) and leases (5) are pure value-adds over existing store primitives; iTerm2
(6) is independent reach; the HTTP transport (4) is last because it is the biggest
new surface and the only item that revises the threat model — it should ship on top
of a retry-safe, well-traced core, not before it.

> Caveats carried forward from the v0.2 sweep: the market-comparison axis was partly
> rate-limited and remains under-sourced (ROADMAP-v0.2 closing note). The
> *structural* items here (1, 2, 3, 5) rest on weave's own code and the well-documented
> agent hook / MCP contracts and are safe to commit to; the *reach* items (4, 6) should
> be re-validated against current MCP-transport and iTerm2-automation docs before
> implementation, since both target moving external contracts.
