# WL-039 — Idle notification deduplication (atm-core parity)

**Plan against:** worktree `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch` off `origin/develop`).
**Status:** plan only — do not implement.

## Goal

When a sender fires a *new* idle/notification ping at a recipient while an **earlier
unread idle ping from the same sender to the same recipient** is still outstanding,
the older ping is auto-superseded (stamped `messages.superseded_by = new_id`) so it
drops out of the recipient's unread inbox and nudge/wake surface — only the latest
"still waiting" ping survives. This is atm-core "idle notification dedup": idempotent
"are you there / still waiting" pings must not pile up N-deep. **Hard boundary:** this
must NEVER dedup a real message (`weave send` / `weave_send`) and must NEVER dedup two
*distinct* notifications — dedup applies only to messages the sender explicitly marked
as idle/notification pings, addressed to the *same* recipient, from the *same* sender,
still *unread*.

## Architecture decision (reuse, don't reinvent)

Reuse the **WL-037 supersede mechanism** wholesale (one-handler reuse law):
`messages.superseded_by` already gives exactly the semantics we need — predecessor is
hidden from every unread/peek/inbox/nudge query (`superseded_by IS NULL` filter is
applied uniformly in `unread_count_conn`, `peek_oldest_unread_conn`, `inbox`, etc.),
kept+flagged in history/search, sender-only authz, chain-forming. **No new hide
mechanism. No change to any unread/nudge query.** Dedup = auto-stamp `superseded_by`
on the prior unread idle ping when a new idle ping arrives.

### The marker problem (the one real design point)

`notify` and `send` write **byte-identical** message rows today — both go through
`Store::send`; there is no `kind`/type column. So "is this an idle ping?" cannot be
inferred. We must add an **explicit opt-in marker** so dedup can only ever fire on
idle pings and never on real content. Two options — **RECOMMEND Option A**:

- **Option A (RECOMMENDED) — new nullable `messages.kind TEXT` column (additive,
  default NULL == "message"), set to `'idle'` only on the idle-notify path.**
  Dedup matches `WHERE sender=? AND recipient=? AND kind='idle' AND superseded_by IS NULL
  AND <unread>`. Self-documenting, queryable, parity-grade (atm-core models a message
  *kind*), and the column is also the natural home for future typed pings. Costs one
  additive nullable column mirrored in both backends + the WL-037-style `ALTER TABLE
  ADD COLUMN` migration (O(1), reads back NULL on old rows). This matches exactly how
  WL-037 added `superseded_by`.
- **Option B — subject sentinel (no schema change).** Treat a reserved subject token
  (e.g. `subject == model::IDLE_PING_SUBJECT`) as the idle marker. Zero migration, but
  fragile (a user could collide with the sentinel; not introspectable; couples dedup to
  a string convention). Only fall back to B if the team wants strictly zero schema
  delta this cycle.

The rest of the plan assumes **Option A**. If the implementer chooses B, drop the
schema/migration rows below and replace the `kind='idle'` predicate with the subject
sentinel check; everything else (trigger point, the new Store method, tests, docs) is
identical.

### Scope guard on the trigger (decision 4)

Dedup is opt-in at the call site, not automatic on every notify. Add an explicit
`--dedup-idle` flag (CLI) / `dedupIdle: true` property (MCP `weave_notify`) — or, if the
team prefers, make *all* `notify` calls idle-marked by default (notify is already
"fire-and-forget, no reply expected", which is exactly the idle-ping shape). **Open
question below.** Either way, `weave_send` / `Cmd::Send` are NEVER touched — only the
notify path can ever set `kind='idle'` and call the dedup method.

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/store.rs` | store (← core/model) | (A) add `kind` to `SCHEMA` messages table + `column_exists` migration block (mirror WL-037 pattern ~L2246); (B) extend `Store::send` *or* add idle-aware insert so the notify path can stamp `kind='idle'`; (C) add new trait method `supersede_prior_idle(&self, sender, recipient, new_id) -> Result<usize>`; (D) impl it on `SqliteStore` | The dedup query + marker live in the store layer, no I/O upward |
| `weave-core/src/store_libsql.rs` | store | Mirror **all** of (A)–(D): schema, migration, send-marker, `supersede_prior_idle` impl | Dual-backend: `Store` trait change must compile + behave identically under `--features libsql` |
| `weave-core/src/model.rs` | model (no I/O) | If marker carried as a typed value: add a small `MessageKind`/`IDLE` const (and `IDLE_PING_SUBJECT` const if Option B). Keep pure | Marker constant belongs with the other model constants; no I/O |
| `weave/src/main.rs` | main (bin) | In `Cmd::Notify` handler (~L2982): after `store.send(...)` returns `mid`, if idle-marked, call `store.supersede_prior_idle(&from, &to, mid)`; add the `--dedup-idle` arg to the `Notify` clap variant (or make notify idle by default — see open Q). Post-stamp ordering mirrors the WL-037 `--supersedes` post-stamp (~L2950) | CLI trigger point; the supersede call is post-send, best-effort-ordered |
| `weave-mcp/src/mcp.rs` | mcp (← inject ← core) | In `tool_notify` (~L818): mirror main.rs — stamp `kind='idle'` on the send and call `supersede_prior_idle` after `mid`. Expose `dedupIdle` in the meta-tool catalog op schema **only** (NOT a new standing tool — WL-051 budget); reuse the existing `weave_notify` op | MCP trigger point; zero standing-token cost |

> Note: how the notify path stamps `kind='idle'` (extend `send` signature vs. a
> dedicated `send_idle`/post-update) is an implementer micro-decision. Prefer the
> **least-invasive** that keeps `Store::send`'s 6-arg shape stable for all existing
> callers (e.g. a post-insert `UPDATE messages SET kind='idle' WHERE id=?` inside a new
> wrapper, or a 7th `kind` param defaulted at call sites). Whatever is chosen must be
> identical in both backends.

## Dual-backend? — YES

Schema + migration + the new `supersede_prior_idle` method + any `send` marker change
must be mirrored in **both** `weave-core/src/store.rs` (default `sqlite`) and
`weave-core/src/store_libsql.rs` (`--features libsql`). Mirror points:
- `SCHEMA` messages DDL: add `kind TEXT` (nullable, no default → NULL == "message").
- Migration: `if !column_exists(conn, "messages", "kind") { ALTER TABLE messages ADD COLUMN kind TEXT; }` — exactly the WL-037 `superseded_by` pattern (store.rs ~L2250).
- `supersede_prior_idle` impl: the dedup `UPDATE ... SET superseded_by=?new WHERE sender=? AND recipient=? AND kind='idle' AND superseded_by IS NULL AND id<>?new AND NOT EXISTS(reads for recipient)`. Both backends, parameterized `params!`.
- Verify both compile: `cargo build` and `cargo build --no-default-features --features libsql`; run the libSQL test suite.

## Invariants in scope

- **Parameterized SQL** — the dedup `UPDATE` and migration must use bound `params!`;
  no string-interpolated sender/recipient. (`store.rs` / `store_libsql.rs`)
- **No-shell / argv-only** — not directly touched (no new subprocess), but the notify
  path still must not pass user text to a shell; unchanged. (`main.rs`, `mcp.rs`)
- **Input caps** — marker carries no new unbounded user input; `kind` is an internal
  enum literal, never user-supplied free text. `id_valid`/`check_ident` on
  sender/recipient unchanged. (`store.rs`)
- **stdout discipline (MCP)** — `tool_notify` change must keep all logging on stderr;
  only the JSON-RPC verdict frame on stdout. (`mcp.rs`)
- **Token-light MCP surface (WL-051/ADR-0003)** — `dedupIdle` goes in the `weave_notify`
  op's catalog schema, NOT a new standing tool. The `standing_mcp_surface_is_within_
  token_budget` test must stay green. (`mcp.rs`)
- **Censorship/DoS guard (WL-037 carry-over)** — `supersede_prior_idle` must scope to
  rows where `sender = caller` (you can only supersede your OWN prior idle ping); it
  must never let session X dedup session Y's pings. This is the same authz spine as
  `Store::supersede`. (`store.rs` / `store_libsql.rs`)

## Test layers required (docs/TESTING.md §8)

- **Unit (store.rs / store_libsql.rs)** — co-located `#[cfg(test)]`, mirror the
  WL-037 tests (~L10112):
  1. `supersede_prior_idle_replaces_prior_unread_idle` — send two idle pings a→b;
     after the 2nd + dedup, b's unread inbox contains ONLY the latest; the first is
     `superseded_by = id2`, hidden from `inbox`/`peek_oldest_unread`/`unread_count`.
  2. `idle_dedup_never_touches_real_messages` — a→b real `send` then a→b idle ping:
     the real message stays unread (NOT superseded); only idle-kind rows are eligible.
  3. `idle_dedup_only_supersedes_unread` — if the first idle ping was already READ by b,
     a second idle ping does NOT supersede it (only *unread* predecessors are replaced).
  4. `idle_dedup_scoped_to_same_sender_recipient` — a→b and c→b idle pings: a's new ping
     supersedes a's prior, never c's; and a→b ping never touches a→z.
  5. `idle_dedup_authz_self_only` — `supersede_prior_idle(caller=c, ...)` cannot stamp a
     row whose `sender != c`.
  6. Run all of the above under **both** backends (the libSQL suite runs the same unit
     tests via the shared trait, or mirror them in store_libsql's test module per repo
     convention).
- **Integration (`tests/integration.rs`)** — drive the compiled binary:
  `two_notify_idle_pings_leave_one_unread` — `weave notify --from a --to b --dedup-idle`
  twice, then `weave inbox --from b` shows exactly one; `weave history` still shows both
  (flagged). Plus a negative: a `weave send` between the two pings remains unread.
- **MCP (`McpServer` test in mcp.rs or tests)** — call `weave_notify {dedupIdle:true}`
  twice via the server; assert the recipient's `weave_inbox` returns one; assert the
  failure/edge path (e.g. dedup on a recipient with no prior idle ping is a clean no-op
  returning `0`). Confirm `weave_send` with the same body twice does NOT dedup.
- **Drift/budget guard** — confirm `standing_mcp_surface_is_within_token_budget` still
  passes (dedupIdle adds no standing tokens).
- **No new proptest required** unless the marker introduces a new invariant; the
  self-only authz is covered by the unit test above (parallels WL-037, which added no
  proptest).

## Docs to sync (same PR)

- **CHANGELOG.md** — under `[Unreleased]`, a `WL-039` entry mirroring the WL-037 entry
  style: "Idle notification dedup (WL-039): a new idle ping from a sender supersedes that
  sender's prior *unread* idle ping to the same recipient (reuses `superseded_by`;
  hidden-from-unread, flagged-in-history); opt-in via `--dedup-idle` / `dedupIdle`;
  additive nullable `messages.kind` column mirrored across both backends; never dedups
  real messages or another sender's pings."
- **docs/REPOWIRE-PARITY.md** — add a row under §1 (near the WL-037 supersede row L62):
  `| Idle notification dedup (atm-core) | weave notify --dedup-idle / weave_notify {dedupIdle} | ✅ HAVE | WL-039; reuses messages.superseded_by; idle-kind + same sender/recipient + unread only; sender-scoped |`.
- **ARCHITECTURE.md** — extend the supersede/`superseded_by` description (the section
  documenting WL-037 hide-from-unread) with one paragraph: idle dedup is an *automatic*
  supersede on the notify path, gated by `kind='idle'` + same (sender,recipient) + unread;
  note the real-message safety boundary. Add `messages.kind` to the schema description if
  the schema is documented there.
- **CONTRIBUTING.md** — only if it enumerates Store methods or message kinds; otherwise
  no change.
- **docs/TESTING.md** — if it maintains a per-WL test inventory, add the WL-039 cases.

## Edit order (dependency-respecting)

1. `weave-core/src/model.rs` — add the `kind` constant / `MessageKind::IDLE` (and
   `IDLE_PING_SUBJECT` if Option B). Pure, no deps.
2. `weave-core/src/store.rs` — schema `kind` column + migration; idle-marking on the
   notify insert path; `supersede_prior_idle` trait method + SqliteStore impl; unit tests.
3. `weave-core/src/store_libsql.rs` — mirror 2 exactly; mirror unit tests.
4. Build both backends green (`cargo build`; `cargo build --no-default-features --features libsql`).
5. `weave/src/main.rs` — `--dedup-idle` arg on `Cmd::Notify`; post-`send` call to
   `supersede_prior_idle`.
6. `weave-mcp/src/mcp.rs` — `dedupIdle` in `weave_notify` catalog schema; dedup call in
   `tool_notify`; keep standing surface unchanged.
7. `tests/integration.rs` + MCP test — the cross-binary cases.
8. Docs sync (CHANGELOG, REPOWIRE-PARITY, ARCHITECTURE, TESTING).
9. Full gate: `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
   `cargo fmt --all --check`, plus the libSQL clippy/build/test and `sign` combos.

## Risks / open questions

1. **MUST NOT dedup distinct real messages — the core safety boundary.** Dedup is
   eligible ONLY for rows explicitly marked `kind='idle'` (set solely by the notify
   path) AND same sender AND same recipient AND still unread. Two *different*
   notifications are still both idle-kind and same-(sender,recipient) — by design the
   newer supersedes the older (that IS the feature: "still waiting" pings collapse to
   the latest). The implementer must confirm the product intent matches: idle dedup
   collapses *all* of a sender's outstanding idle pings to one, regardless of body
   text. Real `send` messages are categorically excluded by the `kind` predicate.
2. **Opt-in trigger vs. default-on (decision to resolve).** Should dedup require
   `--dedup-idle`/`dedupIdle:true`, or should ALL `notify` calls be idle-marked and
   auto-deduped (notify is already "no reply expected", i.e. inherently idempotent)?
   RECOMMEND **opt-in this cycle** (smaller blast radius, explicit; a follow-up can flip
   the default once proven). Implementer/leader to confirm.
3. **Marker mechanism — Option A (`kind` column) vs Option B (subject sentinel).**
   RECOMMEND A. The plan's schema/migration rows assume A; switch to B only on an
   explicit zero-schema-delta directive.
4. **Read-state timing.** `supersede_prior_idle` must check unread against the `reads`
   table for the *recipient*, consistent with `unread_count_conn`. Confirm the dedup and
   the count share the same unread definition so a just-read ping isn't superseded.
5. **Idempotency-key interaction.** If a notify carries an `idempotency_key` and the key
   already exists, `send` returns the existing id (no new row) — dedup must be a no-op in
   that case (new_id == old_id). Add an `id<>?new` guard in the dedup UPDATE (already in
   the query above) and assert it in a unit test.
