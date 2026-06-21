# weave — Architecture

`weave` is a single static Rust binary that is the **Rust-native superset of
repowire** — a full agent-to-agent **orchestration mesh** for coding-agent
sessions (Claude Code and friends). Sessions message each other over a shared
mailbox and — when a recipient is running inside a terminal multiplexer —
**push** the message into that recipient's live pane via a native, paste-safe
injector. But messaging is only the seed: weave coordinates real work through
**structured asks/answers/acks**, **broadcast** and **ask-many** fan-out,
**tool-permission gating**, a **durable job board**, **advisory leases**,
**orchestrator turn-state**, a **review queue**, **scheduling**, **agent
memory**, optional **ed25519 signing**, and optional **LLM thread
summarization** — **70 `weave_*` MCP tools** with full CLI parity. No Python, no
daemon (the DB *is* the broker), no external dependency on `repowire`.

This document describes the modules, the `Store` trait and its backends, the
native injector, the no-daemon push model, lifecycle-hook auto-delivery, the
data model, the threat model, and how weave compares to the two prior tools on
this box (`mcp-broker` and `repowire`). For the mission and the full surface
read §0 first; the early "message each other + inject" framing described the
v0.1.0 seed, not today's mesh.

---

## 0. Mission: the repowire-superset orchestration mesh

weave is the **DEFINITIVE Rust-native SUPERSET of repowire — MORE than repowire,
not less** — in one dependency-light static binary, Python-free. Three
properties are non-negotiable and shape every section below:

- **dependency-light** — one small static binary; heavyweight deps (the
  libSQL/tokio tree, the LLM client) live behind feature flags only.
- **no-daemon** — there is no relay process; the SQLite/libSQL file is the
  broker, and any mux CLI can push into any pane, so the *sender* injects.
- **token-light** (ADR-0003, a first-class invariant peer of dependency-light) —
  the standing context cost of the MCP surface must stay small regardless of how
  many capabilities exist; *adding a feature must not add standing tokens*.

### The shipped surface (WL-001..033, merged)

The orchestration mesh is **already built** — 70 `weave_*` MCP tools, each with a
CLI equivalent, backed by the `Store` trait on both backends:

| Subsystem | What it does | Where |
|---|---|---|
| Messaging + inject | send/notify/reply/inbox/history/thread; native push, paste-safe, 5 muxes + iTerm2 | `weave-inject`, `mcp.rs`, `store.rs` §3 |
| Asks / answers / acks | tracked correlation-ID ask/answer/ack, structured question types | `asks`/`ask_groups` tables §6 |
| Broadcast / ask-many | fan-out notify + ask to all online peers in a circle | `mcp.rs`, §6 |
| Permissions | PreToolUse tool-approval gating, deny-by-default on timeout | `permission_*`, §7 |
| Job board | durable poll-only create/claim/update/result/cancel | `jobs` table §6 |
| Leases | advisory path leases, TTL expiry, conflict detection, pre-commit guard | `leases` table §6 |
| Orchestrator | circles + orchestrator role, turn-state machine, co-orchestrator | `peers.turn_state`, §6 |
| Review queue | track PR review state across peers | `review_queue` table |
| Scheduling | one-shot + recurring scheduled deliveries, drift-safe `tick` | `schedules` table |
| Agent memory | filesystem-backed scoped memory (global/project/persona/orchestrator) | `memory.rs` |
| Summarization | LLM thread summaries, cached in-store (`llm` feature) | `llm.rs`, `summaries` table |
| Signing | optional ed25519 signed sender identity + rotation/revocation | `sign.rs`, §6/§10 |
| Federation / Tier-2 | read-only multi-store federation + cross-store / cross-machine pull | §9, §10 |

### The mission gaps (in scope, not dropped)

weave deliberately dropped repowire's *Python* daemon and human surfaces; the
capabilities themselves remain in scope, to land **Rust-native**:

- **Agent spawn/kill** — `weave_spawn_peer` / `weave_kill_peer`, argv-only,
  per-mux, birth-cert identity, two-layer gated (trusted child program + cwd
  allowlist) — **shipped in WL-047** (§3 spawn/kill table, §7 spawn allowlist).
- **Rust-native human surfaces** — read-only web dashboard + Telegram / Slack
  bridges over `weave-mcp/http.rs`, server-rendered HTML + SSE, **no Next.js/
  Python/async runtime**, behind a `--features surfaces` flag — **shipped in
  WL-048** (ADR-0004; see "Human surfaces" below). WL-052 extends multi-surface
  parity further.
- **Governed web reach** — ✅ **shipped (WL-049, ADR-0002 accepted).** weave is
  the governance plane for the separate `obscura` browser binary: behind a
  default-OFF `--features obscura`, it spawns `obscura mcp` (argv-only, no shell)
  and speaks JSON-RPC over its stdio as a hand-rolled MCP client; all 35
  `browser_*` ops are reachable through ONE token-light `weave_web` dispatcher +
  `weave web` CLI, **deny-by-default** + SSRF-guarded, gated by the existing
  permission/lease/job primitives. **NO V8/tokio/obscura crate in weave's core**
  (zero new default deps). See "Governance plane: stealth web access" below.
- **token-light surface** — **WL-050 (done):** the 70+ eager flat MCP tools are
  replaced by the `weave` **meta-tool** (search/describe/call/list) as the default
  standing surface (≈ a few hundred tokens, zero capability loss), with an eager-flat
  fallback (`WEAVE_MCP_EAGER=1`). **WL-051 (done):** `token-light` is now a
  first-class invariant with a CI-enforced standing-token budget
  (`MAX_STANDING_TOOLS_BYTES` ≈ 2k tokens, guarded by
  `standing_mcp_surface_is_within_token_budget`) — adding a capability must not add
  standing tokens. **WL-052 (foundation done):** the multi-surface parity matrix
  (`docs/MULTI-SURFACE-PARITY.md`) proves CLI + MCP are at full parity; remaining
  human-surface write-parity is tracked as **WL-052a** (dashboard write) / **WL-052b**
  (bot commands). Decided in
  **ADR-0003** (`.handoff/decisions/ADR-0003-token-light-multi-surface.md`).

The provable have/superset/gap parity matrix against repowire's inventory is
`docs/REPOWIRE-PARITY.md` (**WL-046**); the **multi-surface** parity matrix —
every capability mapped onto CLI / MCP / dashboard / bots, with the remaining
human-surface write-parity tracked as WL-052a/b — is `docs/MULTI-SURFACE-PARITY.md`
(**WL-052**, ADR-0003): CLI and MCP are at **full** parity; the dashboard (read-only)
and bots (relay) are the v1 baseline. Structurally, the four-crate workspace below is **interim** —
single-crate remains the goal (WL-043).

---

## 1. Workspace map

`weave` is organized as a Cargo workspace. The default build still produces one
static binary (`weave`), but the code is split into crates so the core types,
store, injector, and MCP server can be reused and tested independently.

```
weave-core/          library: model + config + Store trait + both backends + sign
  src/model.rs         core types + helpers (no I/O); incl. the Tier-2 Intent
  src/config.rs        config file + env overlay
  src/sign.rs          OPTIONAL Ed25519 sign/verify + keyfile (cfg(feature="sign"))
  src/store.rs         Store trait + bundled SQLite backend (cfg(feature="sqlite"))
  src/store_libsql.rs  feature-gated libSQL/Turso backend (cfg(feature="libsql"))
  src/memory.rs        agent memory store (write/read/search/list/delete)
  src/export.rs        pure `render_mailbox_html` + the centralized `html_escape` (no I/O)
  src/archive.rs       pure uncompressed-USTAR writer/reader + `safe_entry_name` traversal guard (no I/O, no deps)
  src/session.rs       pure WL-040 session-export (de)serialize: envelope structs + to_json/from_json + synth key (no I/O)
  src/llm.rs           OPTIONAL chat-completion client for thread summarization (cfg(feature="llm"))
  src/testenv.rs       test-only env lock / guard helpers
weave-inject/        library: native multi-mux injector + `Injector` trait
  src/inject.rs        pure command tables + runner
weave-mcp/           library: MCP stdio JSON-RPC 2.0 server (weave_* tools)
  src/mcp.rs           `serve<I: Injector>` — generic over the injector trait
  src/http.rs          OPTIONAL HTTP surface
weave/               binary crate: CLI, setup, hooks, git tagging, harness
  src/main.rs          clap CLI; wires core + inject + mcp
  src/git.rs           best-effort git session tagging
  src/setup.rs         `weave setup` / `weave uninstall`
  src/harness.rs       `weave harness ide-merge-ide` Codex 7-layer orchestration
  tests/               black-box integration / security / property tests
  benches/weave_bench.rs  criterion throughput benchmarks
```

Dependency direction (top depends on bottom):

```
weave ──▶ weave-mcp ──▶ weave-core
  │           │            ▲
  │           └────────────┤
  └──▶ weave-inject ───────┘
              │
              └──▶ weave-core
```

`weave-core` has no upward dependencies. `weave-inject` depends only on
`weave-core`. `weave-mcp` depends on `weave-core` + `weave-inject`. The `weave`
binary wires all three together and owns I/O-heavy glue (`git.rs`, `setup.rs`,
CLI). The optional `sign` module lives in `weave-core`; the `libsql` backend is
also in `weave-core`.

`config` is the lowest layer above `model`: `store`'s liveness (`is_alive` →
`this_host`), federation (`federated_*` → `peer_db_paths`), and Tier-2 delivery
(`pull_from_store` → `pull_from_paths`) read it downward. The direction never
reverses. The optional `sign` module sits just above `config` (it reads the config
dir for the keyfile); `store` depends down on it for verify-on-commit. The Tier-2
consent nudge is fired **caller-side** in `main`/`mcp`, so there is **no
`store → inject` edge** (§10).

### `model.rs` — core types, no I/O

- `Message { id, ts, sender, recipient, subject: Option, body }`
- `Intent { id, ts, to, to_host, from, subject: Option, body, sig }` — a Tier-2
  cross-store delivery intent: a directed message the sender deposits in its **own**
  outbox for a recipient that lives in another store (§10). `id`/`ts` are the
  sender's local values (the receiver dedups on the source `id` and re-stamps `ts`
  on commit); `sig` is the optional Ed25519 signature (empty unless `--features
  sign`).
- `Peer { name, mux, target, cwd: Option, last_seen, pid: Option<i64>, host }` —
  a session that has registered itself plus where (if anywhere) it can be
  injected. `pid`/`host` are captured at registration so liveness can be checked
  by probing the owning process (see §6); both are additive (`#[serde(default)]`)
  so older rows deserialize cleanly.
- `now() -> i64` — UNIX seconds; timestamps are stored as integers so weave
  needs **no date crate**.
- `fmt_ts(i64) -> String` — formats UNIX seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC)
  using Howard Hinnant's civil-from-days algorithm, again avoiding a date crate.
- Broadcast set: `BROADCAST = ["all","*","everyone","broadcast"]`,
  `is_broadcast(&str) -> bool`, and `BROADCAST_SQL` (the same list as a SQL
  literal tuple). `BROADCAST_SQL` is **derived from the same constant list** so
  the Rust check and the SQL filter can never drift. Its values are compile-time
  constants (never user input), so embedding them as SQL literals is safe.

### `config.rs` — configuration

`Config { session, backend, db, nudge_template, libsql_url, libsql_auth_token,
retention_secs, peer_dbs, pull_from, inject_pulled, allow_inject_from,
strict_verify }`, all `Option`. `Config::load()` reads `~/.config/weave/config.toml`
(`$XDG_CONFIG_HOME` honored) if present, then overlays environment variables:
`WEAVE_SESSION`, `WEAVE_BACKEND`, `WEAVE_DB`, `WEAVE_LIBSQL_URL`,
`WEAVE_LIBSQL_AUTH_TOKEN`, `WEAVE_RETENTION_SECS`, `WEAVE_PEER_DBS`,
`WEAVE_PULL_FROM`, `WEAVE_INJECT_PULLED`, `WEAVE_ALLOW_INJECT_FROM`,
`WEAVE_STRICT_VERIFY` (env wins over file; the list vars *union* onto the file
list). Helpers:

- `backend()` → defaults to `"sqlite"`.
- `db_path()` → config/`WEAVE_DB` override, else
  `$XDG_DATA_HOME/weave/messages.db` (default `~/.local/share/weave/messages.db`).
- `default_db_path()` → the XDG default path, used by `doctor` to flag a
  non-default `WEAVE_DB` (the `db_is_default` field).
- `nudge(from, body)` → the live-injection nudge text, from `nudge_template`
  (with `{from}`/`{body}` substituted) or a built-in default that embeds the body.
- `this_host()` → a stable per-machine host label (`$HOSTNAME` → first line of
  `/etc/hostname` → `"local"`), trimmed, control-char-stripped, and capped at
  `MAX_HOST_LEN` (128). Used as the `peers.host` value (§6) so liveness only
  probes a PID for a peer on *this* host.
- `peer_db_paths()` → the validated, deduped, capped (`MAX_PEER_DBS` = 16) list
  of extra read-only store paths for federation (§9), unioned from `peer_dbs` and
  `WEAVE_PEER_DBS`; the local `db_path()` is dropped (no self-federation).
  Default (unset) ⇒ empty ⇒ identical-to-today behavior.
- `pull_from_paths()` → the Tier-2 **delivery** sources (§10), validated/deduped
  with the same discipline and cap (`MAX_PULL_FROM` = 16) as `peer_db_paths`, but
  keyed off the **distinct** `pull_from` list. Default (unset) ⇒ empty ⇒ no
  cross-store delivery.
- `inject_pulled()` → the Tier-2 consent toggle, **defaulting to `true`** (the one
  place the original default-off is intentionally flipped). `false` ⇒ pure
  queue-only delivery.
- `allow_inject_from_paths()` / `inject_allowed_from(&Path)` → the optional finer
  gate narrowing which pull sources may fire the consent nudge; unset ⇒ "same as
  the pull set".
- `strict_verify_override()` → the **tri-state** Tier-2 signed-identity strictness
  override (`Some(true)` = force strict everywhere, `Some(false)` = advisory
  everywhere for the unsigned/unknown path, `None` = no override ⇒ the
  trust-set-aware default decides per sender). `trust_set()` / `revoked_set()` →
  the validated, deduped, capped (`MAX_TRUST` = 64) receiver-local fingerprint
  lists; `trust_set_configured()` → whether the trust set is non-empty (which makes
  strict the default for trusted senders). All four are only consulted on the
  pull/commit path of a `--features sign` build; the collapsed-bool `strict_verify()`
  is retained for back-compat. Default (everything unset) ⇒ advisory ⇒
  identical-to-today.

### `store.rs` — persistence

Owns the `Store` trait (§2), the bundled `SqliteStore`, the SQL schema, the
`ONLINE_TTL_SECS` presence window (900 s), `is_online(last_seen)`, and the
liveness layer on top of it: `is_alive(peer)` and `pid_alive(pid)` (§6). It also
owns the read-only federation aggregator — `open_readonly`, the
`federated_peers` / `federated_sessions` / `federation_status` free functions, and
the pure `merge_peer_views` / `merge_session_views` dedup/tie-break (§9). For
Tier-2 (§10) it owns the `outbox` / `pull_cursor` / `keys` tables, their additive
trait methods, and the `pull_from_store` / `commit_pulled` free functions that
read each allowed source read-only and commit addressed intents into the local
inbox (owner-only-writes; the consent nudge is fired caller-side, so this layer
takes no `inject` dependency).

### `inject.rs` — native injector

The `Mux` enum, `Target { mux, id }`, environment detection, the pure
per-mux command tables, and the runner. Detailed in §3. Also exposes the pure
`capability(&Target) -> Capability` verdict (`Live` / `RegisteredNotAlive` /
`NotInjectable`) composed from `injectable()` + the liveness probe — this is what
`weave connect` / `weave_connect` report (§4). It adds no new spawn path: the
probe is the existing fail-open `target_alive` and the verdict is a pure value.

### `mcp.rs` — MCP server

A newline-delimited JSON-RPC 2.0 server over stdio implementing `initialize`,
`ping`, `tools/list`, `tools/call`, and empty `resources/list` / `prompts/list`.
It exposes the `weave_*` operations and performs the live nudge-inject on send.
stdout is reserved for protocol frames; **all logging goes to stderr**.

**Token-light progressive disclosure (WL-050 / ADR-0003).** `tool_catalog()` is the
canonical registry of every `weave_*` operation (name, description, inputSchema) and
the single source `call_tool` dispatches against. The **standing** surface returned by
`tools/list`, however, is *not* the full catalog: by default it is **one** tool — the
`weave` **meta-tool** — so the standing context cost stays bounded (≈ a few hundred
tokens) no matter how many operations exist (the `token-light` invariant). The full set
is reached on demand through the meta-tool's modes:
`search {query}` (find ops), `list` (enumerate), `describe {name}` (one op's schema),
`call {name, arguments}` (invoke it). `call` routes back through `call_tool`, so it
preserves **every** guard — the safe-HTTP destructive-op gate is re-applied to the inner
op, and it refuses to target `weave` itself (no recursion). A backward-compatible
**eager-flat** mode (`WEAVE_MCP_EAGER=1`) restores the complete flat `tools/list` for
harnesses that require flat tools — no capability or compatibility lost.
`weave_attach` (zero-restart self-adoption — re-capture the pane and upsert the
caller's own peer row) and `weave_connect` (the §4 capability verdict) sit
alongside the messaging tools; the peers/sessions/doctor tools also surface
read-only federation (§9) when extra stores are configured.

### Human surfaces (`--features surfaces`, WL-048 / ADR-0004)

Three optional **human** surfaces, all Rust-native and behind a single
`--features surfaces` flag (default OFF ⇒ the default binary links **zero** extra
deps; the bots reuse the same optional `reqwest` blocking+rustls client `llm`
carries, so Cargo unifies it to **one** copy). These are **CLI subcommands, not
MCP tools** (ADR-0003 token-light), so the standing MCP surface is unchanged.

- **Read-only web dashboard** (`weave dashboard`) — server-rendered HTML + SSE
  over the **existing** hand-rolled `std::net` HTTP transport in
  `weave-mcp/http.rs`. The render layer is the new **pure** `dashboard.rs`
  (`html_escape`, `render_dashboard(snapshot, now, host)`, `sse_event` /
  `sse_keepalive`, a `route(method, path)` classifier) — socket-free and DB-free,
  so it unit-tests with no listener. `http.rs::serve_dashboard(port, token,
  store_factory)` owns the listener and spawns a **short-lived `std::thread` per
  accepted connection** (each opens its own read-only `Store` handle — `Store:
  Send` but not `Sync`) so a long-lived `GET /events` SSE stream cannot starve
  other requests (still **no async runtime**). The dashboard is **read-only**
  (`GET /` HTML, `GET /events` SSE — never mutates), **localhost-bound**, and
  **bearer-gated** (WL-022). `handle_connection` (the MCP port) also answers those
  GET routes under the feature, with the **POST/JSON-RPC path byte-identical**.
- **Telegram bridge** (`weave telegram`) / **Slack bridge** (`weave slack`) —
  `weave/src/telegram.rs` / `slack.rs`, **poll-only v1** (no inbound webhook
  server). Each factors a **pure** payload-builder (`telegram_send_payload` /
  `slack_post_payload`) and inbound-parser (`parse_telegram_update` /
  `parse_slack_message`) tested with no network, plus a blocking `reqwest` poll
  loop on the CLI thread: inbound human messages become `Store::send` from the
  configured `bridge_identity` (idents sanitized + `check_ident`-validated, bodies
  capped at `MAX_BODY` first); outbound, the loop relays the bridge inbox to the
  chat. Bot tokens are SECRETS (config/env, Debug-redacted, never logged).

### Governance plane: stealth web access (`--features obscura`, WL-049 / ADR-0002)

weave does **not** link a browser. Behind a default-OFF `--features obscura`, it
governs the separate `obscura` binary via a **spawn-and-speak MCP-client** model:

- **`weave-core/src/webpolicy.rs`** (core, no I/O): the pure deny-by-default
  decision (`WebPolicy::decide` over the 35-op `WEB_OPS` allow-list) and the
  **SSRF/loopback URL validator** (`check_url` / `host_is_internal` — denies
  loopback / `localhost` / link-local incl. `169.254.169.254` / RFC1918 / `*.local`
  / bare-IP, plus encoded-loopback forms (decimal/hex/octal/trailing-dot/IPv4-mapped),
  unless `obscura_allow_internal`), plus `MAX_WEB_ARG_LEN` caps. Pure ⇒
  unit-tested exhaustively like `model.rs`.
- **`weave-mcp/src/obscura.rs`** (mcp): a minimal hand-rolled **MCP client**. It
  resolves `obscura` to a trusted absolute path (`weave_inject::resolve_trusted` —
  never ambient `$PATH`), spawns `obscura mcp [--stealth] [--proxy P]
  [--user-agent UA]` **argv-only**, and speaks newline-delimited JSON-RPC over the
  child's stdio (`initialize` → `notifications/initialized` → `tools/call`),
  matching replies by monotonic id and extracting `result.content[0].text` /
  mapping `isError`. Built on `std::io` + `serde_json` — **no tokio, no async, no
  new dependency**. One cached child per process (lazy spawn, reuse), bounded
  per-op read deadline + line cap, and a `Drop`/`stop()` that kills+reaps the
  child (no zombies). The child's stdout is a pipe weave READS (never re-emitted on
  weave's own stdout); its stderr is `null`'d and never logged.
- **`weave-mcp/src/mcp.rs`** — ONE token-light dispatcher `weave_web {action, args,
  describe?}` (in `DANGEROUS_TOOLS`): resolve caller → `WebPolicy` gate → optional
  lease (`reserve_lease`/`release_lease`) → optional audit job
  (`create_job`/`update_job`) → forward to obscura → return. Per-op schemas are
  fetched on demand (`describe`), so the standing tool table grows by ~1, not 35
  (ADR-0003). Governance **reuses the existing permission/lease/job Store methods**
  — no new Store method, no schema change, dual-backend unaffected.
- **`weave/src/main.rs`** — `weave web <op> [--url …] [--arg k=v] [--list]
  [--stop] [--lease-ttl N] [--audit]` routes through the SAME `tool_web` path (CLI
  parity, zero standing tokens).

### `setup.rs` — host wiring (multi-provider)

`run_provider(exe, provider)` wires weave into the selected coding-agent host;
`uninstall_provider(provider)` reverses it. The `Provider` enum has four variants
(`Claude` default, `Codex`, `Gemini`, `Aider`), surfaced by the CLI flag
`weave setup --provider <…>`. **All four share the same write discipline** — only
the target file and serialization format differ:

| Provider | Target file | Merge strategy | Mechanism |
|---|---|---|---|
| `Claude` | `~/.claude/settings.json` | JSON `hooks.{event}[]` merge + `claude mcp add` | **confirmed** (baseline, unchanged) |
| `Codex` | `~/.codex/config.toml` | line-based TOML merge of the top-level `notify = [...]` argv key (no `toml` dep) | partially confirmed (`notify`→drain) |
| `Gemini` | `~/.gemini/settings.json` | the SAME Claude `hooks.{event}` JSON merge (`merge_hooks_at`) | ⚠ unconfirmed key — scaffold-with-caveat |
| `Aider` | `~/.aider.conf.yml` | append a hand-templated `weave-hook:` YAML stanza (no `serde_yaml` dep) | ⚠ limited surface — scaffold-with-caveat |

Shared **merge primitives** are factored to take a target path: `read_json`/
`write_json_atomic` (JSON providers), `write_bytes_atomic` (every provider — atomic
temp+rename, one-time `<name>.weave.bak` 0o600 snapshot, mode-preserving), and the
read/parse-guard that on a **non-NotFound** read error ABORTS without writing (never
truncates a populated config). **Every destructive rewrite is read-back-verified
(WL-041):** after the atomic write, the per-provider writer re-opens and re-parses
the file and asserts weave's intended entry is present (merge) or absent (prune)
AND — for the JSON providers — that every pre-existing **foreign** hook captured
before the write survived, returning a descriptive `Err` (naming the `.weave.bak`
recovery path) on mismatch rather than a silent `Ok`. The Codex/Aider read-backs
assert their weave-owned line/marker landed (merge) or is gone (prune). This
mirrors the WL-035 backup-archive read-back; `weave restore` does the same for the
restored `config.toml`/`settings.json` bytes. The provider config files are
**sidecar config** (the host's own settings), not build/runtime inputs of weave —
so this adds **no language drift and no new dependency**. (Legacy note, ignore: "not yet
implemented" line and return `Ok(())`. They exist so the CLI surface
(`weave setup` / `weave uninstall`) is stable while the real implementation —
registering the MCP server and merging lifecycle hooks into
`~/.claude/settings.json` idempotently — lands later. Until then, wiring is
manual (see README).

### `main.rs` — CLI + glue

`clap`-derived CLI. Loads `Config`, opens the store, and dispatches subcommands.
`resolve_me()` resolves this session's identity:
**explicit flag > config/`$WEAVE_SESSION` > basename(cwd)**. `setup`/`uninstall`
are handled before the store is opened (they need no DB).

---

## 2. The `Store` trait and its backends

`Store` is the backend-agnostic, **object-safe** persistence interface, so the
app holds a `Box<dyn Store>` and selects the backend at runtime from config.

```rust
pub trait Store: Send {
    fn send(&self, sender:&str, recipient:&str, subject:Option<&str>, body:&str) -> Result<i64>;
    fn inbox(&self, me:&str, include_read:bool, mark_read:bool, limit:i64) -> Result<(Vec<Message>, i64)>;
    fn unread_count(&self, me:&str) -> Result<i64>;
    fn history(&self, me:&str, peer:Option<&str>, limit:i64) -> Result<Vec<Message>>;
    fn sessions(&self) -> Result<Vec<(String,i64,i64)>>;   // (name, unread, last_ts)
    fn total_messages(&self) -> Result<i64>;
    fn clear_inbox(&self, me:&str) -> Result<usize>;
    fn clear_all(&self) -> Result<i64>;
    fn register_peer(&self, name:&str, mux:&str, target:&str, cwd:Option<&str>) -> Result<()>;
    fn get_peer(&self, name:&str) -> Result<Option<Peer>>;
    fn list_peers(&self) -> Result<Vec<Peer>>;
    fn backend(&self) -> &'static str;
    // Tier-2 cross-store delivery (§10), all additive:
    fn enqueue_intent(&self, to:&str, to_host:&str, from:&str, subject:Option<&str>, body:&str, sig:&str) -> Result<i64>;
    fn list_outbox(&self, for_recipient:&str, since_id:i64, limit:i64) -> Result<Vec<Intent>>;
    fn outbox_all(&self, limit:i64) -> Result<Vec<Intent>>;
    fn pull_cursor_get(&self, source:&str) -> Result<i64>;
    fn pull_cursor_set(&self, source:&str, last_id:i64) -> Result<()>;
    // Tier-2 signed identity (§10), additive (the identity_keys table is always present):
    fn register_key(&self, identity:&str, pubkey:&str) -> Result<()>; // APPENDS (multi-key)
    fn get_key(&self, identity:&str) -> Result<Option<String>>;        // most-recent shim
    fn get_keys(&self, identity:&str) -> Result<Vec<String>>;          // ALL keys, oldest-first
    fn remove_key(&self, identity:&str, pubkey:&str) -> Result<bool>;  // prune a retired key
    fn list_keys(&self) -> Result<Vec<(String,String)>>;
    // P6 delivery observability, additive (the delivery_log table is always present):
    fn record_delivery(&self, ref_id:i64, ref_kind:&str, to_peer:&str, stage:&str, outcome:&str) -> Result<()>; // metadata-only INSERT; NEVER injects
    fn list_delivery(&self, ref_id:i64, limit:i64) -> Result<Vec<DeliveryTrace>>; // oldest-first, bounded by MAX_DELIVERY_ROWS
}
```

**P6 — delivery observability (`delivery_log`).** weave's read receipts capture
*read-state*; the `delivery_log` table captures *transport-state*. It is a
**metadata-only, SECRET-FREE** append-only trace — columns are exactly
`(id, ref_id, ref_kind, to_peer, stage, outcome, ts)`, **never** the body, subject,
sig, or any token. `ref_kind ∈ {message, notify, ask}`, `stage ∈ {queued, injected,
inject_failed, not_injectable, drained}`, `outcome ∈ {ok, fail}` are `model` enums
(stored as TEXT, validated via `as_str`/`from_str`). Rows are written **caller-side,
best-effort, AFTER the inject** at every send/notify/ask/answer point (and a `drained`
stage at the hook `prompt` mark-read drain) by a thin `record_delivery_best_effort`
wrapper that logs to stderr and never sinks delivery — exactly the live-nudge seam, so
there is **no `store → inject` edge**: `record_delivery` records the outcome it is
*passed* and never injects. Reads (`list_delivery` / `weave_delivery` / `weave delivery`)
are bounded by `MAX_DELIVERY_ROWS`; lifetime is bounded by the **existing `gc()`
retention** (gc prunes `delivery_log WHERE ts < cutoff` in the same pass — no new
sweeper). `weave_notify` is a thin no-reply primitive over `store.send` + the P1 honest
verdict (it does not fork send and opens no tracked thread); broadcast notify is deferred.
Mirrored across both backends with a guarded additive migration; a legacy DB gains the
table on open.

Key semantics:

- **`inbox`** returns `(messages, remaining_unread)`. Messages are those whose
  recipient is `me` *or* a broadcast alias, excluding `me`'s own sends, newest
  first internally but returned oldest-first for natural reading. When
  `mark_read` is set, each returned message id is recorded in `reads` for `me`
  inside a transaction.
- **Per-reader read tracking**: a broadcast is delivered exactly once *per
  reader*, because read state lives in `reads(message_id, reader)` rather than a
  flag on the message.
- **`clear_inbox`** marks `me`'s unread as read (non-destructive). **`clear_all`**
  truncates `messages` and `reads` (destructive; the MCP tool requires
  `confirm:true`).
- **`register_peer`** is an upsert keyed on `name`, also refreshing `last_seen`
  (used for presence via `is_online`).

### Backends

| Backend | Module | Default? | Sync model | Use |
|---|---|---|---|---|
| `sqlite` | `store.rs` (`SqliteStore`) | yes | synchronous (rusqlite, bundled) | local mailbox |
| `libsql` | `store_libsql.rs` (`LibsqlStore`) | no — `--features libsql` | async (tokio) | cross-machine / Turso replicas |

`SqliteStore::open` creates parent dirs, opens the file, sets
`busy_timeout=30s`, `journal_mode=WAL`, `synchronous=NORMAL`, and applies the
schema idempotently (`CREATE TABLE IF NOT EXISTS`). The on-disk SQLite format is
**libSQL-compatible**, so the same file is portable between backends with no
migration — the file is the broker.

**`Store::snapshot_to(dest)`** (WL-035, mirrored across both backends) writes a
consistent point-in-time copy via a **parameterized `VACUUM INTO ?1`** (the
destination path is *bound*, never inlined) — a fully-checkpointed copy, never an
`fs::copy` of a live WAL DB — then read-back-verifies it (re-opens the snapshot
read-only and counts). The **remote libSQL** backend has no local file to vacuum,
so it bails with a clear message. This is the snapshot primitive `weave backup`
archives (and the GAP-2 hardening makes the `weave export` write report its path
on failure).

### Three distinct "export" surfaces — do not conflate

weave has **three** export-shaped commands with very different jobs:

| Surface | Command | Form | Purpose |
|---|---|---|---|
| **WL-034** | `weave export` | self-contained HTML | offline *presentation* of a mailbox (viewer) |
| **WL-035** | `weave backup` / `restore` | USTAR tar (binary DB snapshot) | **byte-exact, host-local** backup/restore |
| **WL-040** | `weave session export` / `import` | **canonical JSON** | **logical, portable, versioned** session interchange |

The one-line rule: **WL-040 is logical + portable + versioned; WL-035 is byte-exact
+ host-local.** WL-040 (`weave-core/src/session.rs` pure (de)serialize +
`weave/src/session.rs` I/O handler) serializes one identity's **messages** (read via
`Store::history`) plus its **mesh memory** (the filesystem-backed scoped memory) into
a schema-versioned JSON envelope, and re-imports it into a *different* instance whose
row ids will not match. **Import reuses `Store::send`** — no new backend method, no
schema change — so id-remap is free (fresh local ids) and re-import is idempotent
(dedup on `idempotency_key`, with a deterministic synthetic key
`wl040:<identity>:<id>` for keyless legacy messages). Identity is remapped via `--as`.
Tracked **asks are replayed faithfully** (WL-040b) via the dual-backend
`Store::import_ask` — a deliberate out-of-order materializer that inserts an ask
row DIRECTLY in its exported `AskState` (open / answered / acked), bypassing the
create→answer→ack lifecycle (the question/answer message rows already exist from the
message-import pass), with `question_msg_id`/`answer_msg_id` **remapped** to the
freshly minted local message ids (resolved from `Store::send`, which returns the
existing id on a dedup hit). Broadcast-ask **groups** round-trip too: the envelope
carries the `ask_groups` parent anchors (read via `Store::list_ask_groups`),
replayed via `Store::import_ask_group` **before** the child asks so `parent_id`
linkage survives. Both new methods are mirrored in `store.rs` (named `params!`) and
`store_libsql.rs` (positional `params(vec![...])`, 15-column INSERT order pinned to
`row_to_ask`'s indices) and dedup (skip-existing) on the remapped triple /
`parent_id`, so re-import is idempotent. A **dangling** ask (its message absent from
the export) is skipped+counted, never linked broken; the `reply_to` chain pointer is
NULLed (it references a regenerated source ask id). **Peers are excluded by design**
(host/mux/birth-cert-local liveness, a takeover hazard elsewhere). The import file is **untrusted external input**: every field is bounded
(`check_ident`, `MAX_BODY`, subject cap, id-shape) before any write, all writes go
through parameterized `Store::send`, no shell is spawned, the format embeds no path
fields, and `--in`/`--out` are traversal-guarded (the `backup.rs` discipline). The
full contract lives in `docs/FORMAT-session-export.md`.

`open_store()` in `main.rs` picks the backend from `Config::backend()`. Selecting
`libsql` in a binary built without the feature fails with a clear message rather
than silently falling back, so configuration mistakes are loud. The default
build (no features) stays green and pulls in no tokio/libSQL tree.

---

## 3. Native injector design

The injector delivers text into a *running* agent session's terminal pane by
driving the terminal multiplexer (or control-capable terminal) it lives in. It
is weave's own first-class component — **no Python, no repowire**.

### Per-mux command tables

`Mux` enumerates the supported backends, each mapping to a CLI binary and an
environment variable used for detection:

| `Mux` | CLI binary | Detect env var | Target meaning |
|---|---|---|---|
| `Tmux` | `tmux` | `TMUX_PANE` (+ `TMUX` socket) | pane id (e.g. `%3`); `socket` = server path from `$TMUX` |
| `Zellij` | `zellij` | `ZELLIJ_SESSION_NAME` + `ZELLIJ_PANE_ID` | session name + pane id |
| `Kitty` | `kitten` | `KITTY_WINDOW_ID` | window id |
| `Wezterm` | `wezterm` | `WEZTERM_PANE` | pane id |
| `Screen` | `screen` | `STY` | session |
| `None` | — | — | not injectable |

`commands_for(target, text) -> Vec<Vec<String>>` is a **pure function**: given a
target and text it returns the exact argv vectors to run, with no side effects
and no multiplexer required. That purity is what makes the injector unit-testable
on a build host with no mux present — every backend has a test asserting its
exact argv, and there are 38 tests total across the crate (22 unit + 16 integration).

`detect_target()` probes the environment most- to least-specific (tmux first,
because a process can be inside tmux *and* a terminal, and the multiplexer owns
the input line) and returns a `Target`. `Target::injectable()` is true only when
the mux is not `None` and the id is non-empty. `Target::from_peer(&Peer)` rebuilds
a target from a registered peer's stored `(mux, target)`.

### Session tag acquisition (`src/git.rs`)

Where `detect_target()` answers *how to reach* a session, `src/git.rs` answers
*what the session is*: it derives the session's **repo name**, **branch**, and a
canonical **worktree id** from its cwd at registration (and refreshes them on
`weave scan`), so a `peers` row is self-describing across the mesh. It sits at the
inject tier — pure parsers plus a no-shell argv `git` runner — and is **never** a
build/link dependency: `git` is invoked as an **external trusted binary** (resolved
via `inject::resolve_trusted`, an absolute path from a trusted dir, never ambient
`$PATH`), so weave stays one dependency-light Rust binary.

Acquisition is best-effort and total — a git/fs failure or a non-git cwd yields
empty tags and **never sinks registration** (the hook hot path) — and writes are
self-only (owner-only-writes: a session only ever tags its own row):

- **`worktree_id`** comes from a pure `.git`-file parse *first* (zero subprocess):
  a linked worktree's `<cwd>/.git` is a file holding
  `gitdir: …/.git/worktrees/<name>/.git`, and `parse_worktree_id_from_gitdir`
  recovers `<name>` as the canonical id. A main (non-linked) worktree has a `.git`
  *directory* → the literal `"(main)"` sentinel. No `.git` at all → empty (and the
  subprocess is skipped entirely).
- **`branch`** is `git rev-parse --abbrev-ref HEAD`, with a
  `git worktree list --porcelain` parse fallback (`parse_worktree_porcelain`,
  matched to this worktree's path) when the former is blank/detached.
- **`repo`** is the basename of `git rev-parse --show-toplevel`
  (`repo_name_from_toplevel`).

The argv `git` runner (`git_capture`) copies `inject::run_capture`'s discipline:
`Command::new(<trusted git>).args([...]).current_dir(cwd)` with a wall-clock
timeout + kill and `Stdio::null()` stderr/stdin — explicit argv, never `sh -c`,
never a built command string, so cwd/repo/branch text never reaches a shell. The
store seam (`sanitize_tag`, §7) bounds and control-strips every tag on write.

### Paste-safe submission

Submitting the message (pressing "Enter") is the subtle part. Modern TUIs such
as Claude Code run in **bracketed-paste** mode, where a naive Enter after literal
text can be swallowed or misread as a TUI key (this was a documented `repowire`
bug — injection triggering a cancel mid-tool-call). Each backend therefore uses
the paste-safe submission idiom for its terminal:

- **tmux** — three commands: send the literal text
  (`send-keys -t <pane> -l -- <text>`), then **close bracketed paste** with the
  hex sequence `ESC [ 2 0 1 ~` (`send-keys -t <pane> -H 1b 5b 32 30 31 7e`), then
  send `Enter`. Closing the paste before Enter is what stops the TUI from
  treating the newline as a cancel. **WL-053:** when the peer was registered from a
  non-default tmux server, its captured socket is threaded as `tmux -S <socket> …`
  on every command (inject/spawn/kill/liveness) so they reach the originating
  server instead of `$TMUX`'s default; a socket-less peer keeps the historical argv.
- **zellij** — write the literal chars (`action write-chars <text>`), optionally
  scoped to a specific pane via `--pane-id`, then write byte 13 (`action write 13`).
- **kitty** — match the target window by id and send the text, then send a
  carriage return as a separate `send-text`.
- **wezterm** — `send-text --no-paste` avoids bracketed paste entirely, then
  submit with a carriage return.
- **screen** — `-X stuff "<text>\r"` injects the string plus carriage return in
  one shot.

### Running and graceful degradation

`inject(target, text) -> Result<bool>`:

- returns `Ok(false)` when the target is not injectable (mux `None` / empty id);
- checks the mux binary is on `PATH` via `have(bin)` and `bail!`s with a clear
  message if missing;
- runs each command in order and `bail!`s if any exits non-zero (e.g. the pane
  is gone).

Errors are never fatal to messaging: the message is already persisted, so
callers treat an injection failure as "fall back to next-turn delivery."

### Spawn / kill (WL-047)

The injector also **launches and terminates** agents, not just nudges them.
`spawn_commands(mux, cwd, name, cert, argv_child, window)` and
`kill_commands(target)` are **pure exact-argv builders** — the same discipline as
`commands_for`: argv vectors only, never a shell string, every attacker-influenceable
positional (cwd + each child argv element) after an end-of-options `--` where the
mux's CLI accepts one. The runners `spawn(...) -> SpawnOutcome` and
`kill(target) -> bool` resolve the mux binary by absolute path through the existing
trusted-path runner, validate the child argv against `spawn_arg_ok` (length cap
`MAX_SPAWN_ARG_LEN`, count cap `MAX_SPAWN_ARGS`, reject NUL/control), thread
`WEAVE_SESSION` / `WEAVE_BIRTH_CERT` / `WEAVE_CIRCLE` into the child via
`Command::envs`, and capture the new pane/window id where the mux echoes one.

| `Mux` | spawn (pane default; `window` → window/tab) | kill | id echoed? |
|---|---|---|---|
| `Tmux` | `tmux split-window -P -F '#{pane_id}' -c <cwd> -- <argv…>` (window: `new-window`) | `tmux kill-pane -t <id>` | yes (`%n`) |
| `Zellij` | `zellij action new-pane -- <argv…>` (window: `new-tab`) | `zellij delete-session --force <name>` (coarse) | no |
| `Kitty` | `kitten [--to <sock>] @ launch --type tab --cwd <cwd> --env WEAVE_SESSION=<name> [--env WEAVE_BIRTH_CERT=<cert>] -- <argv…>` (window: `--type os-window`) | `kitten [--to <sock>] @ close-window --match id:<id>` | yes (int) |
| `Wezterm` | `wezterm cli spawn --cwd <cwd> -- <argv…>` (window: `--new-window`) | `wezterm cli kill-pane --pane-id <id>` | yes (int) |
| `Screen` | `screen -dmS <name> <argv…>` (child program at idx 3) | `screen -S <name> -X quit` (coarse) | no |
| `ITerm2` / `None` | `vec![]` (unsupported) | `vec![]` (unsupported) | — |

**The fail-open rule** mirrors the liveness probes: a mux that cannot cleanly spawn
(`ITerm2` / `None`) or cannot echo a usable target id (`Zellij` / `Screen`) is **not
an error**. For an id-echoing mux the parent pre-registers the peer row with the
minted cert and the captured target; for a non-echoing mux the parent registers
nothing and relies on the **child's own self-registration** at its first
`weave hook session` (env-derived target + the threaded cert). Kill is exact where
the mux can address a pane (`kill-pane`/`close-window`) and **coarse** — a whole
detached session teardown — where it cannot (`Zellij`/`Screen`), documented as
best-effort, never a correctness guarantee. The child program (`argv[0]`) is
rewritten to its **trusted absolute path** inside the spawn argv (the mux binary
stays a bare name for the trusted-path runner to resolve), and the cwd is gated by
the spawn allowlist (§7) — the two-layer trust gate.

---

## 4. The no-daemon push model

weave has **no long-running process**. Push works because every supported
multiplexer can target an arbitrary pane/session from *any* process:

- `tmux send-keys -t <pane>` reaches any pane on the tmux server;
- `zellij --session <name> action write-chars` reaches any zellij session;
- kitty/wezterm/screen have equivalent addressable-target CLIs.

So the **sender injects directly into the recipient's registered pane** — there
is no relay and no broker process in the middle. The `peers` table is the
registry that maps `name → (mux, pane/session id)`, captured from the
environment (`$TMUX_PANE`, `$ZELLIJ_SESSION_NAME`/`$ZELLIJ_PANE_ID`, etc.) at `SessionStart`.

**Registration / adoption seam.** Registration is an upsert keyed on the
session's *own* resolved identity, so it can run at three moments:
`weave hook session` at `SessionStart`, the explicit `weave register`, and
`weave attach` / `weave_attach` — the **zero-restart adoption** path. A session
that started outside a multiplexer (or before `weave setup`) can re-capture its
current pane and upsert its own row at any time without restarting; the upsert
binds the caller's own validated identity, so there is no argument path to
overwrite another peer's row. All three capture the process `pid` + `host`
(§6) for liveness.

**Connect handshake.** Before sending, a caller can probe reachability with
`weave connect --to <peer>` / `weave_connect`. It looks up the peer, builds a
`Target`, and reports the pure `inject::capability()` verdict — `Live`,
`RegisteredNotAlive`, or `NotInjectable`. This **reuses the existing injector**
(no new injector, no new spawn path): the verdict is computed from `injectable()`
plus the fail-open liveness probe. A not-alive or non-injectable verdict is **not
an error** — those messages still arrive via the recipient's next store drain;
only a non-existent peer is an error.

Send path (MCP `weave_send`, mirrored by the `weave send` CLI):

1. `store.send(from, to, subject, body)` persists the message and returns its id.
2. If `to` is **not** a broadcast and `store.get_peer(to)` yields an injectable
   peer, weave builds a `Target::from_peer` and calls `inject()` with the nudge
   text (default `[weave] message from <from>: <body> (run weave_inbox to read)`,
   or the configured `nudge_template` with `{from}`/`{body}` substituted). The
   nudge carries the message body so the recipient sees the content the instant
   it lands; the persisted copy still arrives on their next hook drain.
3. The tool result reports whether a live nudge was injected, whether the peer
   had no injectable target, or that inject failed and the message will arrive on
   the recipient's next turn.

Broadcasts are never injected (only persisted) — they fan out to every reader via
inbox/hook delivery, not by pushing into N panes.

The DB is the only shared state. Concurrent senders are serialized by SQLite's
WAL mode + busy timeout. An optional presence daemon (`weave daemon start|stop|status|run`)
provides live online/offline status and lifecycle eviction, but it is **not required**
for messaging or injection — when stopped, the system degrades transparently to the
existing TTL heuristic.

---

## 5. Lifecycle-hook auto-delivery

When weave is wired into Claude Code's lifecycle hooks, the CLI subcommand
`weave hook <event>` runs at session events. Each hook reads the event JSON on
stdin (for `cwd`), resolves the session identity, and acts:

| Hook event | Claude Code trigger | Action |
|---|---|---|
| `session` | `SessionStart` | `detect_target()` + `register_peer_full(name, mux, id, cwd, pid, host)` — the session becomes an injectable peer, capturing its PID + host for liveness (§6). Then sets `turn_state = pending_first_turn` (P5). |
| `prompt` | `UserPromptSubmit` | Drain unread (`inbox` with `mark_read`) and print each to **stdout**, which Claude Code folds into the agent's context. Then sets `turn_state = working` (P5). |
| `stop` | `Stop` | Same drain as `prompt`. Then sets `turn_state = idle` (P5). |
| `notification` | `Notification` | Sets `turn_state = awaiting_input` (P5 — activated this previously-reserved arm). |

This is the **graceful-degradation** path: even with no multiplexer present (so
no live injection is possible), unread messages are still delivered into the
agent's context on its next turn or when it stops. The two delivery channels
compose — an injectable peer gets an instant nudge *and* the full message on its
next hook drain; a non-injectable session gets only the hook drain.

The P5 `turn_state` write each hook performs is **best-effort** — it runs *after*
the drain/registration above, and a failure is logged to stderr and swallowed
(`if let Err`/`let _`, never `?`-propagated, the gc/git-tags precedent), so a
presence-update failure can never sink message delivery. It is UPDATE-only on the
caller's **own** row, so it is not gated on explicit identity (a guessed name
worst-case touches zero rows; it can never consume a foreign inbox).

---

## 6. Data model

Tables created idempotently (`CREATE TABLE IF NOT EXISTS`):

```sql
messages    (id INTEGER PK AUTOINCREMENT, ts INTEGER, sender TEXT, recipient TEXT,
             subject TEXT NULL, body TEXT)
reads       (message_id INTEGER, reader TEXT, ts INTEGER, PRIMARY KEY(message_id, reader))
asks        (id TEXT PRIMARY KEY, question_msg_id INTEGER NOT NULL, answer_msg_id INTEGER NULL,
             asker TEXT NOT NULL, askee TEXT NOT NULL, subject TEXT NULL,
             state TEXT NOT NULL, reply_to TEXT NULL, close_note TEXT NULL,
             opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL, closed_ts INTEGER NULL,
             parent_id TEXT NULL)                                  -- ask-many child link (P2, additive)
ask_groups  (parent_id TEXT PRIMARY KEY, asker TEXT NOT NULL, subject TEXT NULL,
             body TEXT NOT NULL, opened_ts INTEGER NOT NULL, target_count INTEGER NOT NULL)
peers       (name TEXT PRIMARY KEY, mux TEXT, target TEXT, cwd TEXT NULL,
             last_seen INTEGER, pid INTEGER NULL, host TEXT NOT NULL DEFAULT '',
             repo TEXT NOT NULL DEFAULT '', branch TEXT NOT NULL DEFAULT '',
             worktree_id TEXT NOT NULL DEFAULT '',
             circle TEXT NOT NULL DEFAULT 'default',                -- visibility-scoping group (P4, additive)
             role TEXT NOT NULL DEFAULT 'peer',                     -- peer | orchestrator (P4, PeerRole enum)
             turn_state TEXT NOT NULL DEFAULT '',                   -- '' | pending_first_turn | working | awaiting_input | idle (P5, TurnState enum; '' = unknown)
             description TEXT NOT NULL DEFAULT '',                  -- free-form self-description (P5, ≤200 chars, control-stripped)
             description_ts INTEGER NOT NULL DEFAULT 0)             -- description set-time; read-time TTL anchor (P5; 0 = unset)
-- Tier-2 cross-store delivery (§10):
outbox      (id INTEGER PK AUTOINCREMENT, ts INTEGER, to_peer TEXT, to_host TEXT NOT NULL DEFAULT '',
             from_peer TEXT, subject TEXT NULL, body TEXT, sig TEXT NOT NULL DEFAULT '')
pull_cursor   (source TEXT PRIMARY KEY, last_id INTEGER NOT NULL)
keys          (identity TEXT PRIMARY KEY, pubkey TEXT NOT NULL)   -- DEPRECATED shadow (#7)
identity_keys (identity TEXT NOT NULL, pubkey TEXT NOT NULL, added_ts INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (identity, pubkey))                    -- multi-key registry (#7)
revocations   (id INTEGER PK AUTOINCREMENT, ts INTEGER NOT NULL, fp TEXT NOT NULL,
               identity TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT '',
               kind TEXT NOT NULL DEFAULT 'enforced')             -- observed-revocation audit log (#11)
-- Poll-only durable job board (P3, §8):
jobs        (id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', description TEXT NOT NULL DEFAULT '',
             kind TEXT NOT NULL DEFAULT 'general', state TEXT NOT NULL, state_reason TEXT NULL,
             phase TEXT NULL, prompt TEXT NULL, progress_note TEXT NULL,
             progress_events_json TEXT NOT NULL DEFAULT '[]',     -- append-only [{at,note,state,phase}]
             creator TEXT NOT NULL, owner TEXT NULL, assignee TEXT NULL, circle TEXT NULL,
             correlation_id TEXT NULL, source_kind TEXT NULL, source_id TEXT NULL, scope TEXT NULL,
             visibility TEXT NOT NULL DEFAULT 'circle',
             attempt_id TEXT NULL,                                -- current claim/fencing token (att_<…>)
             deadline_at INTEGER NULL, expires_at INTEGER NULL,
             result_summary TEXT NULL, result_json TEXT NOT NULL DEFAULT '{}',
             error_json TEXT NOT NULL DEFAULT '{}', artifacts_json TEXT NOT NULL DEFAULT '[]',
             cancel_requested INTEGER NOT NULL DEFAULT 0, cancel_requested_by TEXT NULL,
             cancel_requested_ts INTEGER NULL, cancel_reason TEXT NULL,
             opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL, completed_ts INTEGER NULL)
            -- + indexes: state, (owner,updated_ts), (assignee,updated_ts), (circle,updated_ts)
```

- **`messages`** — the append-only mailbox. `recipient` is a session name or a
  broadcast alias. **`superseded_by`** (WL-037) is an additive nullable column
  (`ADD COLUMN`, mirrored across both backends; **NULL == not superseded**, so a
  legacy DB upgrades in place inert). A sender **replaces** a prior message with
  `weave send --supersedes <id>` / the `supersedes` property on `weave_send`: the
  predecessor is stamped `superseded_by = <successor id>`. The read-semantics rule is
  **hide-from-unread, flag-in-history**: every unread/inbox/nudge path adds
  `AND superseded_by IS NULL` (so a reader never sees a superseded message as a fresh
  unread — if the sender supersedes before the recipient drains, only the successor
  surfaces), while `history`/`thread`/`search` **retain and flag** the superseded row
  for audit. Chains are supported (only the tail is unread). Authorization is
  **sender-only** — `supersede` looks up `old_id`'s sender and rejects unless it equals
  the caller (a censorship/DoS guard; advisory like the rest of `from` until `sign`).
  Supersede is **replacement** and is orthogonal to `in_reply_to` **threading**.
  **`expires_at`** (WL-038) is a second additive nullable column (`ADD COLUMN`,
  mirrored across both backends; **NULL == permanent**, so legacy DBs upgrade inert).
  A sender marks a message **ephemeral** with `weave send --ttl <secs>` / the `ttl`
  property on `weave_send`: the store stamps the **absolute deadline**
  `expires_at = ts + ttl` (storing the deadline — not the relative ttl — makes every
  sweep a single `WHERE expires_at <= now()`, mirroring `leases.expires`). The TTL is
  capped at `MAX_MSG_TTL_SECS = 86400` (24h), validated by `ttl_valid` at both the CLI
  and MCP seams (the `expires_at = ts + ttl` deadline uses `saturating_add`, so the cap
  also forecloses an overflow). The lifecycle is **delete-on-sweep, not filter** — an
  expired ephemeral message must not be reconstructable, so the row (and its `reads`)
  are **deleted**, not merely hidden. Expiry is enforced two ways: (1) folded into the
  existing `gc()` pass (the WL-016 fold-into-gc precedent — gc deletes
  `expires_at IS NOT NULL AND expires_at <= now()` in the same transaction, even when
  `ts >= cutoff`), and (2) a new `sweep_expired_messages()` called **opportunistically**
  at the top of every unread/read entry point (`inbox`/`peek_oldest_unread`/`history`/
  `search`/`inbox_since`), so expiry holds even with no explicit gc. As belt-and-
  suspenders for the tiny between-sweep window, every read surface also carries an
  `AND (expires_at IS NULL OR expires_at > now())` guard, so an expired-but-unswept row
  is still invisible. Ephemeral messages carry through the **cross-store** intent path
  via an additive `outbox.ttl` (the *relative* ttl, since the receiver re-stamps `ts` on
  commit) — re-stamped to an absolute `expires_at` on pull-commit beside the priority
  carry. A broadcast may be ephemeral: delete-on-sweep removes the row and all its
  per-reader `reads` together.
  **`kind`** (WL-039) is a third additive nullable column (`ADD COLUMN`, mirrored
  across both backends; **NULL/`'normal'` == ordinary message**, so legacy DBs
  upgrade inert). It marks an **idle/notification "still waiting" ping** (`kind =
  'idle'`), set **only** on the notify dedup path. It powers **idle notification
  dedup**, an *automatic* `supersede` on the notify path (reusing the WL-037
  `superseded_by` spine — no new hide mechanism): when a sender fires a new idle ping
  with `weave notify --dedup-idle` / the `dedupIdle` property on `weave_notify`,
  `Store::supersede_prior_idle(sender, recipient, new_id)` stamps the new row
  `kind='idle'` and then stamps `superseded_by = new_id` on the sender's prior
  **unread** idle ping(s) to the same recipient, collapsing a pile of idempotent
  "are you there?" pings to just the latest. Dedup is **opt-in** (the flag /
  property; plain `send` is never touched) and is bounded by a hard **real-message
  safety boundary**: the supersede `UPDATE` only matches rows where ALL hold —
  `kind = 'idle'` (excludes every real message), `sender = caller` (self-only authz,
  the same censorship/DoS guard as `supersede`), `recipient` match, still-unread (the
  same `NOT EXISTS (reads…)` definition as `unread_count`), `superseded_by IS NULL`,
  and `id <> new_id` (so an idempotency-key replay that returns the existing id is a
  clean no-op, never self-supersede). It can therefore never dedup a distinct real
  message or another session's pings.
- **`reads`** — per-`(message, reader)` read state. This is what makes a
  broadcast deliverable exactly once per reader and keeps each session's "unread"
  independent.
- **`asks`** — the **tracked ask/answer/ack** side-table (P1, the first step toward
  weave⊇repowire capability parity). Like `reads` and `revocations`, it is a **mutable
  side-table to the append-only `messages`**: one row per correlation-tracked request,
  keyed by an opaque `correlation_id` (`ask_<rowid>_<nonce>`, minted server-side, charset
  `[A-Za-z0-9_]`, validated by `model::ask_id_valid` before any bind). The actual
  question/answer **text reuses `messages`** (threaded via `in_reply_to`) — `asks` holds
  only the correlation + lifecycle (`asker`/`askee`/`subject`, the `state`, pointers
  `question_msg_id`/`answer_msg_id`, an optional `reply_to` chain link, the `close_note`,
  and `opened`/`updated`/`closed` timestamps) and points at the `messages` rows by id.
  `state` is a **monotonic** lifecycle, `open → answered → acked` (never backward),
  enforced by the pure `model::AskState::can_transition` *before* any UPDATE — an illegal
  edge (double-ack, answering an acked thread) is a clean `bail!`, never a panic or a
  silent regression. `ask(reply_to = X)` acks `X` and links the new question into `X`'s
  conversation in the same transaction (so `weave thread` renders the chain). The live
  nudge + the honest delivery verdict (`transport_delivered` / `queued_next_turn` /
  `recipient_not_injectable`) are computed **caller-side** in `mcp`/`main` by reusing the
  existing `inject::capability` + `inject_mode` return — there is **no `store → inject`
  edge** and **no new dependency**. Always-present plain data; point-to-point only
  (broadcast/cross-store ask are out of P1). Created on every open in **both** backends
  via an additive, guarded, idempotent migration (the `reads`/`revocations` precedent).
- **`ask_groups`** + **`asks.parent_id`** — the **ask-many** parent↔child link (P2, the
  second weave⊇repowire parity epic). `ask_many` fans **one question to an explicit list
  of peers**: it inserts one `ask_groups` parent anchor (`askm_<id>`, validated by
  `model::ask_many_id_valid`) holding the canonical question `body`/`subject`/`asker` and
  the de-duped `target_count`, then creates **one normal P1 `ask` per peer** carrying the
  parent's id in the additive nullable `asks.parent_id` column (NULL for every plain ask
  and every legacy P1-era row). A child **is** a P1 ask — it answers/acks through the
  unchanged `open → answered → acked` lifecycle, no duplicated state machine. `ask_many` is
  **best-effort**: an invalid/unreachable/broadcast peer is skipped with a per-child error
  rather than failing the whole call (it never gets a child row and counts as `failed` at
  read time, so `target_count` keeps the totality `answered + acked + pending + failed ==
  target_count` checkable). The per-child live nudge is fired **caller-side** in `mcp`/`main`
  (the same honest-verdict seam as P1) — still **no `store → inject` edge**. `ask_many_result`
  is a **read-time aggregate**: it enumerates the children `WHERE parent_id = ?1`, rolls up
  their states, lists the pending peers, and classifies `complete | partial | pending` via the
  pure `model::classify_ask_many` — **no background ticker, no stored deadline**. `partial`
  appears only when the caller passes an `age` threshold and an open child has waited at least
  that long. The fan-out is bounded by `MAX_ASK_MANY_TARGETS = 64` (explicit peer list only;
  circles compose in a later epic). Both `ask_groups` and `asks.parent_id` ship as **additive,
  guarded, idempotent migrations in both backends** — a legacy P1-era DB upgrades in place
  (`ADD COLUMN parent_id` defaults NULL; `CREATE TABLE IF NOT EXISTS ask_groups`) with **no
  new dependency**.
- **`peers`** — the injection registry: where each named session can be reached,
  plus `last_seen`, `pid`, and `host` for presence, and the descriptive session
  tags `repo` / `branch` / `worktree_id`. The `pid`/`host` and the
  `repo`/`branch`/`worktree_id` columns are each added by an **additive, idempotent
  migration** (guarded, mirroring the `socket` precedent) in **both** backends, so
  a pre-existing DB upgrades in place and an old row reads `pid:NULL` / `host:""` /
  empty tags. The three tag columns are `TEXT NOT NULL DEFAULT ''` (nullable in
  spirit — empty means "unknown/non-git"), appended after `host` at fixed positions
  8/9/10 so the column order is identical across backends. They are **descriptive
  tags only**, never injection targets. The P4 `circle` / `role` columns are
  appended LAST (positions 11/12) by the same additive idempotent migration in both
  backends; `circle` defaults to the non-empty literal `'default'` (so a legacy row
  classifies into the default circle with no runtime coalesce) and `role` defaults
  to `'peer'`. **circle** is a visibility-scoping group (a label, validated by
  `circle_valid` + `MAX_CIRCLE_LEN=64`, captured at registration from
  `WEAVE_CIRCLE`/config like identity is captured from `WEAVE_SESSION`). **role** is
  a two-variant enum `PeerRole::{Peer, Orchestrator}` stored as TEXT (the
  `AskState`/`JobState` precedent — an enum, never free text). **Orchestrator
  policy:** `role='orchestrator'` is the single per-circle coordinator. It is
  **claimed, never self-asserted** — a registration always inserts `role='peer'` and
  an upsert **omits `role`** so a re-register preserves an existing orchestrator.
  `claim_orchestrator_role(circle?, force?)` runs in ONE transaction: a non-force
  claim while a **different LIVE** holder exists (`role='orchestrator'` AND
  `is_alive`) is **refused** (a clean `ClaimOutcome::Refused`, not an error); `force`
  demotes every other orchestrator in the circle to `peer` and sets the caller. The
  forced demote is the **only cross-row peer write** P4 adds — a single-row UPDATE in
  the caller's **own** local store (never a foreign store, the owner-only-writes
  contract is about cross-STORE writes), and **non-destructive** (a role bit; the
  demoted peer can re-claim) ⇒ **not** confirm-gated. The "live" verdict REUSES
  `is_alive` (no new probe — weave's daemon-free analog of repowire's heartbeat).
  `peers`/`sessions`/`scan` filter by circle caller-side over the merged views
  (federation composes); the default is the caller's circle, an orchestrator caller
  defaults to mesh-wide, `--all-circles`/`circle='*'` is mesh-wide. There is **no
  `store → inject` edge** for circles/roles. The P5 **rich-presence** columns
  `turn_state` / `description` / `description_ts` are appended LAST (positions
  13/14/15) by the same additive idempotent migration in both backends; a legacy row
  reads `turn_state=''` (unknown) / `description=''` / `description_ts=0`, so an
  un-upgraded peer's surfaced output is byte-identical to a pre-P5 weave. **turn_state**
  is a `TurnState::{Unknown, PendingFirstTurn, Working, AwaitingInput, Idle}` enum
  stored as TEXT (`Unknown ⇒ ''`, the `PeerRole`/`AskState` precedent — an enum,
  never free text; `from_str` hard-errors an unknown value at the seam). It is
  **auto-set by the lifecycle hooks** (§5: session→pending_first_turn,
  prompt→working, stop→idle, notification→awaiting_input) and also exposed via the
  explicit `set_turn_state` setter. **description** is a free-form self-set string,
  sanitized through `sanitize_tag(_, MAX_DESC_LEN=200)` (control-stripped, capped on a
  UTF-8 boundary, internal spaces kept — oversized truncates, never errors) and
  stamped with `description_ts = now()` on set (`0` on clear, so a cleared description
  is unambiguously absent). It carries a **read-time TTL** of `DESCRIPTION_TTL_SECS`
  (900 s — the value of `ONLINE_TTL_SECS`, but a **named, independent** constant so a
  description ages out independently of liveness): the pure `model::expire_description(&mut
  Peer, now)` helper (no I/O, no clock of its own — `now` is passed in, totality via
  `saturating_sub`) blanks a description whose `description_ts` is older than the window.
  It is applied at the **store read seam** (`get_peer`/`list_peers`, both backends) right
  after the row map — **daemon-free, no sweeper**, and the **stored row is never mutated**
  (a pure read-time view; the next `set_description` re-stamps). Both setters are
  **owner-only**: an `UPDATE … WHERE name = ?` bound to the caller's resolved identity
  (the `claim`/`attach` precedent), never an arg-supplied target; the only inlined
  turn_state SQL literals come from `TurnState::as_str` (compile-time). There is **no
  `store → inject` edge** for presence, and **no new dependency**.
- **`outbox`** — Tier-2 pending intents the owner queued for recipients in *other*
  stores (§10). Append-only; `id` is the monotonic dedup key the receiver tracks.
  `sig` is empty unless `--features sign` signed the intent.
- **`pull_cursor`** — the receiver's per-source high-water mark on the source's
  `outbox.id`, the idempotency key for pull/commit.
- **`identity_keys`** — the multi-key registry (#7): registered `(identity, pubkey)`
  pairs for signed-identity verification, holding **multiple** keys per identity so
  rotation can OVERLAP (old + new both verify during a window). `added_ts` orders the
  keys (newest-first for the `get_key` shim, oldest-first for `get_keys`/`list`).
  Always-present plain data; the SIGN/VERIFY crypto is `sign`-gated. Created on every
  open in **both** backends via an **additive, guarded, idempotent** migration that
  also copies any legacy single-key `keys` rows in (`INSERT OR IGNORE … SELECT identity,
  pubkey, 0 FROM keys`, keyed on the `(identity,pubkey)` primary key so a re-run is a
  clean no-op). A NEW key is APPENDED (`ON CONFLICT(identity,pubkey) DO NOTHING`); a
  duplicate is a no-op; the per-identity count is capped at `MAX_KEYS_PER_IDENT` (16,
  a store constant — bounds a hostile registry; a duplicate never counts against it,
  and exceeding it returns an error, never a panic).
- **`keys`** — the **deprecated** legacy single-key table (`identity PRIMARY KEY`),
  RETAINED as a shadow (no DROP) for crash-safety and old-binary coexistence. Nothing
  reads it anymore; new writes go ONLY to `identity_keys`.
- **`revocations`** — the **observed-revocation audit log** (#11): an append-only
  record of *when* revocation was exercised, for operator visibility only. A
  `declared` row is written when an operator runs `weave key revoke`; an `enforced`
  row is written (best-effort) when the R1 predicate rejects a pulled signed intent
  that verified only against a revoked key. **Write-on-enforce, never read by the
  verifier** — `verify_pulled_intent` never touches this table, so R1 stays the
  single, absolute, config-driven decision source and the log can never weaken or
  drift from it. An audit-write failure is logged to stderr and swallowed; it cannot
  change the rejection. Always-present plain data; every read/write call site is
  `sign`-gated. Created on every open in **both** backends via an additive, guarded,
  idempotent migration (mirroring `identity_keys`). Secret-free: it stores only full
  fingerprints (`SHA256:<64-hex>`, derived from public keys), public identities,
  source labels, and a `kind`. Surfaced read-only by `weave audit revocations` and
  the (count-only) `doctor` / `weave_doctor` verify summary.
- **`jobs`** — the **poll-only durable job board** (P3, the third weave⊇repowire
  parity epic — see §8). One row per durable unit of work, keyed by an opaque
  `job_<rowid>_<nonce>` id (minted server-side, validated by `model::job_id_valid`
  before any bind). A job carries a **`JobState` lifecycle**, descriptive metadata
  (title/description/kind/owner/assignee/circle/visibility, plus inert nullable
  `correlation_id`/`source_kind`/`source_id`/`scope` for board filtering), an
  **append-only** `progress_events_json` audit log, the terminal `result_summary` /
  `result_json` / `error_json` / `artifacts_json` (all **TEXT JSON**, byte-capped),
  and the cooperative-cancel flags. Timestamps are weave-native `i64` epoch seconds
  (`model::now()`), never ISO strings — `deadline_at` / `expires_at` are
  caller-supplied i64.
  - **`JobState` — the state machine.** The enum is a **forward-compat 11-state
    superset** (`Queued, Dispatching, Delivered, Running, AwaitingInput, Completed,
    Failed, Cancelled, Blocked, Expired, Unavailable`) so a later runner epic needs
    no model migration; **P3's write paths only ever mint the poll-only subset**
    (`queued` on create; claim → `running`; update/cancel produce the rest), while
    `dispatching` / `delivered` / `unavailable` are accepted-on-read and reachable
    only via a generic update. The **terminal set is frozen** —
    `{Completed, Failed, Cancelled, Expired, Unavailable}` — no edge leaves a
    terminal state (idempotent self-noop excepted), and `cancel`/`expire` may
    **interrupt** from any non-terminal state; otherwise transitions are forward
    progress within the active lane. The legality check is the pure
    `model::JobState::can_transition`, run **before any UPDATE** — an illegal edge is
    a clean error, never a panic; the enum-label↔string round-trip is locked by a
    drift-guard test (the BROADCAST-literal discipline).
  - **`attempt_id` fencing (claim → token → stale-rejection).** A worker **claims**
    a job via `claim_job`, which **mints a fresh `attempt_id`** token, assigns the
    job, and moves it to `running` (claim is the *only* path that sets `attempt_id`).
    `update_job` then enforces fencing **in the store** (so CLI and MCP inherit it
    identically): if the row is claimed (non-NULL `attempt_id`), the supplied token
    **must equal** the row's current one, else `Err("stale_attempt")`; an unclaimed
    (NULL) job accepts a tokenless update (pre-claim parking). A **re-claim mints a
    new token**, fencing out the prior worker — the proptest asserts only the latest
    token ever validates.
  - **Cooperative cancel — never a hard delete.** `cancel_job` mirrors repowire: a
    still-`queued` row transitions straight to terminal `cancelled`; an in-flight
    (claimed/running) row only gets the `cancel_requested` flag set, which the worker
    observes on its next poll and honors — no daemon is needed to *request* a cancel.
    P3 has no hard-delete path.
  - **No `store → inject` edge, no new dependency.** P3 is pure DB — `jobs` adds no
    injector call and no crate (`serde_json`, already a dep, carries the JSON TEXT
    columns); the module DAG (`model` ← `store`) is unchanged (the state machine in
    `model`, the lifecycle in `store`). **Runner-only columns are excluded** — the
    lease/runner-owner/attempts-ledger and the cron/schedule/spawn-exec config that
    drive repowire's autonomous JobRunner are **not** carried (deferred to a later
    runner epic); only the single first-class `attempt_id` fencing token is promoted.
    Created on every open in **both** backends via an additive, guarded, idempotent
    migration (the `reads`/`revocations` precedent), so a legacy DB upgrades in place
    and the table is inert plain data in every build.
- The Tier-2 tables are whole **new** tables created on every open in **both**
  backends, so a legacy (pre-Tier-2) DB upgrades in place with no per-column ALTER;
  `identity_keys` additionally absorbs the legacy `keys` rows on first open. The
  `jobs` table follows the same whole-new-table pattern.

### Presence: `liveness_for` / `is_alive` vs `is_online`

`is_online_at(last_seen, now_ts)` is the pure recency guard (within
`ONLINE_TTL_SECS` = 900 s, the single freshness window — there is no separate
presence const). **Presence display now means *alive*, not "wrote recently".**

#### A2 — fail-open by host (named principle)

Presence is governed by one rule the tests cite as **A2**: liveness is
**pid-authoritative on the same host, TTL-only (fail-open) on a remote host**.
weave can probe a process only on the machine it runs on, so:

- **Same host** (`peer.host == this_host()`) with a known PID → the PID is
  authoritative: a dead-but-recent local process reads stale.
- **Remote host** (`peer.host != this_host()`, *including an empty host* — see
  below) → **never pid-probed**; weave fails OPEN to the TTL recency verdict (the
  Turso/libSQL shared-DB case). A remote/legacy peer must never falsely read dead,
  and we never probe a PID that might collide with an unrelated local process.

This is a security/correctness invariant, not a heuristic: there is **no
cross-machine pid/network/ssh/ping probe anywhere** — the only probe is the
same-host `/proc/<pid>` check, gated to the local arm. An *empty* host always
classifies remote because `this_host()` is never empty (it falls back to
`"local"`), so `"" != this_host()` holds and the empty-host row fails open by TTL.

#### `liveness_for` — the pure host-aware classifier

The A2 rule lives in one **pure** function in `store` that takes `this_host` and
`now_ts` as parameters (so it is exhaustively testable with a fixed host/clock —
the only I/O is the same-host PID probe, gated to the local arm):

```rust
pub enum Liveness { AliveLocal, AliveRemote, Stale }

pub fn liveness_for(peer: &Peer, this_host: &str, now_ts: i64) -> Liveness {
    if !is_online_at(peer.last_seen, now_ts) { return Liveness::Stale; }   // recency first
    if peer.host == this_host {
        match peer.pid {
            Some(pid) if !pid_alive(pid) => Liveness::Stale,  // local dead pid ⇒ stale
            _                            => Liveness::AliveLocal, // null pid ⇒ TTL fallback
        }
    } else {
        Liveness::AliveRemote   // remote (incl. empty host): TTL-only, NEVER pid-probed
    }
}
```

- `Liveness::AliveLocal` — same host, within the TTL window, and pid-confirmed
  (or a null-pid TTL fallback, still local).
- `Liveness::AliveRemote` — remote host (incl. empty), within the TTL window,
  liveness presumed by recency only (fail open).
- `Liveness::Stale` — past the TTL window, **or** a same-host row whose known PID
  is dead. `Liveness::token()` returns the stable machine tokens `"alive_local"` /
  `"alive_remote"` / `"stale"`. The pid-confirmed-vs-TTL-presumed nuance is
  surfaced only in the human reason string, not as a fourth variant.

`pid_alive` is a Linux `/proc/<pid>` existence check (no new dependency) and
**degrades to assume-alive** off Linux via `cfg`.

#### `is_alive` delegates (truth table unchanged)

`is_alive(peer) -> bool` is now a thin wrapper —
`!matches!(liveness_for(peer, &this_host(), now()), Liveness::Stale)` — reading the
real `this_host()`/`now()`, so every existing bool call site (`peers`,
`sessions --watch`, `doctor`, the MCP tools) sees **byte-identical** results. The
truth table is unchanged; the enum only adds an observability dimension
(local-vs-remote + reason) on top of the same alive/stale boundary.

#### The liveness reason is surfaced uniformly across all four presence surfaces

`weave scan` (and the `weave_scan` MCP tool) consume `liveness_for` per row to
distinguish remote-host sessions and show *why* a peer is alive — a `<remote>`
marker, a per-row reason string, additive `--json` keys, and a `summary` count
line (see README). Cross-machine liveness inherits the same `ONLINE_TTL_SECS` =
900 s freshness window: a remote peer seen within 15 minutes is presumed alive.

That **same** vocabulary is now surfaced UNIFORMLY across the other three
presence surfaces — `weave peers`, `weave doctor`, and the `sessions --watch`
dashboard (plus the `weave_peers` / `weave_doctor` MCP mirrors) — so all four
read the one classifier and speak one language. This is **display-only**: the
`is_alive` truth table is unchanged (each surface's alive count is still
`!matches!(liveness, Stale)`); no schema, SQL, or `Store`-trait change.

- **`peers`** prints the ` <remote>` marker + `[<reason>]` per row and adds the
  `"liveness"` (token) / `"remote"` (bool) keys to `--json`.
- **`doctor`** computes the three counts in one pass over `views` via
  `liveness_for` and emits a `liveness:` line plus the `--json` keys
  `peers_alive_local` / `peers_alive_remote` / `peers_stale`.
- **`sessions --watch`** classifies each row inside the **pure** render.

Because the dashboard render holds only loose `SessionRow` fields (not a full
`Peer`), `liveness_for` is now a thin wrapper over a field-level seam,
`liveness_from_fields(host, pid, last_seen, this_host, now_ts) -> Liveness`. The
render delegates to it, so the dashboard classifies a `SessionRow`
**deterministically from `(now, this_host)`** with byte-identical results to a
full-`Peer` `liveness_for` call — no behavior change to either path. The render
takes `this_host` (and `now`) as parameters, keeping the pure-render seam intact
(the only env-dependence is the same-host PID probe, gated to the local arm,
exactly as `scan`).

Read paths keep `last_seen` warm: `weave peers` and a long-lived `weave watch`
each refresh presence (heartbeat-on-read, explicit-identity only) so a session
stays visible even with no message traffic.

### Optional presence daemon

The daemon is an **opt-in background process** that writes periodic heartbeats
to the `presence` table (§2) so peers show **Live** status even without message
traffic. It is started/stopped via the CLI (`weave daemon start|stop|status`)
and exposed over MCP (`weave_daemon_start|stop|status`).

- **Daemon loop** (`weave daemon run --me <name>`): every 15 s calls
  `store.heartbeat(name, host, pid)`; every 60 s calls
  `store.evict_stale_presence(30)` to prune rows older than 30 s.
- **PID file** defaults to `$XDG_RUNTIME_DIR/weave/weaved.pid` with a temp
  fallback; overridable via `WEAVE_PIDFILE` for test parallel safety.
- **Idempotent start**: checks the pidfile with an argv-only `kill -0` probe;
  if the recorded PID is alive, start is a no-op.
- **Stop** sends `kill -TERM` (argv-only, no shell) and removes the pidfile.
- **MCP tools** duplicate the small pidfile logic directly (they cannot depend on
  the `weave` bin crate per the layer DAG). They return JSON-shaped text:
  `{"started":true,"pid":N}` / `{"stopped":true}` / `{"running":true,"pid":N}`.
- **No new dependency**: the daemon uses only `std::process::Command` and
  `std::thread::sleep`.

When the daemon is absent, the three-tier liveness resolver (`peer_liveness`)
falls back to `Likely` (TTL recency) and then `Offline`, so presence display is
never broken — the daemon only makes the **Live** tier more accurate.

### Presence dashboard: `weave sessions --watch`

`weave sessions --watch` re-renders a **read-only** presence view of the
federated peers — the same scan model (`federated_peers` joined with `is_alive`),
grouped by `(repo, branch)` — on a fixed interval. The design keeps weave
dependency-light and the loop testable:

- **Pure render seam.** A single pure function
  `render_sessions_dashboard(rows, opts, this_host, now) -> String` does all
  formatting: no I/O, no clock (the `now` is passed in), no sleep. The impure
  watch loop only re-reads a snapshot, calls the pure renderer, prints, and
  sleeps. This mirrors the `commands_for` purity discipline — the renderer is
  unit-testable from hand-built rows against a **fixed `now` + fixed `this_host`**,
  with no store and no terminal. Each `SessionRow` carries `pid` / `last_seen`
  (not a precomputed `alive` bool) so the render classifies liveness itself via
  `liveness_from_fields`, deterministically from `(now, this_host)`.
- **Std-only loop, no new dependency.** The loop is `std::thread::sleep` between
  frames; the in-place redraw is a plain ANSI clear-home literal
  (`\x1b[2J\x1b[H`) gated by `std::io::IsTerminal` on stdout **and** `NO_COLOR` /
  `WEAVE_NO_CLEAR` being unset (otherwise frames are plain, escape-free text). No
  TUI / signal / async crate is introduced — termination is the default SIGINT
  (Ctrl-C), and no raw mode is ever entered, so the terminal cannot be left in a
  bad state. This deliberately mirrors the existing inbox `watch` loop.
- **Read-only.** The loop writes **nothing per tick** — observing presence must
  not perturb it. At most one owner-only self-refresh of the watcher's own row
  runs *once before* the loop (gated on explicit identity, reusing
  `register_peer_full` exactly as `scan` does), never per frame.
- **No store / schema change.** The dashboard consumes already-fetched
  `PeerView` data through the existing backend-agnostic `federated_peers` +
  `is_alive`; there is **no** new `Store` method, no SQL, and no `SessionView`
  change, so both backends are unaffected beyond the shared gate.
- **Bounded iterations for hermetic tests.** `--iterations N` renders exactly `N`
  frames then exits (`0` ⇒ loop forever); the sleep happens *between* frames,
  never after the last, so `--iterations 1` returns immediately. An integration
  test thus drives a single deterministic frame with no hang and no wall-clock
  assertion. The poll `--interval` is clamped in `config` to `[1, 3600]`s
  (`clamp_watch_interval`), reusing the input-cap discipline.

Unread for `me` = messages addressed to `me` or to a broadcast alias, not sent by
`me`, with no matching `reads` row for `me`. Timestamps are UNIX seconds
(`model::now()`), formatted to UTC ISO-8601 only at display time
(`model::fmt_ts`).

---

## 7. Threat model

weave runs locally and trusts the operator of the machine; its mailbox is a
local file readable by that user. The security focus is therefore on **how
injected and stored text is handled**, not on network attackers.

- **No shell, ever.** Every external command is spawned with
  `std::process::Command::new(bin).args(...)` — an explicit argv vector. weave
  never builds a shell command string and never invokes `sh -c`, so message
  bodies and session names cannot be interpreted as shell syntax. There is no
  command-injection surface even if a message body contains `;`, `$(...)`,
  backticks, or quotes.
- **Argument handling is structured.** `commands_for()` places user text as a
  *single argv element* (e.g. tmux's `-l -- <text>` literal mode, where `--`
  ends option parsing so a body starting with `-` is not mistaken for a flag).
  Pure construction means the exact bytes that reach the mux CLI are
  unit-asserted.
- **SQL is parameterized.** All variable values use bound `params!`. The only
  inlined SQL literals are the broadcast aliases, which are compile-time
  constants derived from `BROADCAST` — never user input — so `BROADCAST_SQL`
  cannot be an injection vector.
- **Session tags are sanitized at the store seam.** The cwd-derived `repo` /
  `branch` / `worktree_id` tags pass through `sanitize_tag` inside
  `register_peer_full` in **both** backends (trim → drop control chars →
  char-boundary-safe `take(MAX_*_LEN)`, each 128) before persistence, so a hostile
  or oversized cwd-derived tag is bounded and control-free, is never re-emitted
  verbatim, and is never an injection target (tags are descriptive only). Capture
  is no-shell argv `git` (§3), so the tag text cannot reach a shell either.
- **Injection is a contained side effect.** The worst case of a hostile body is
  the text appearing in another session's pane (a social/UX concern), not code
  execution. A failed or impossible injection degrades to next-turn hook
  delivery; it never crashes the sender, because the message is already
  persisted before injection is attempted.
- **stdout discipline.** The MCP server writes only protocol frames to stdout and
  all diagnostics to stderr, so a malformed log line can't corrupt the JSON-RPC
  stream.
- **Destructive ops are gated.** `weave_clear` with `scope:"all"` wipes every
  session's messages and requires an explicit `confirm:true`; the default scope
  only marks the caller's own inbox read. **Spawn/kill are dangerous tools.**
  `weave_spawn_peer` and `weave_kill_peer` are in `DANGEROUS_TOOLS`, so the safe
  HTTP MCP surface disables them unless started with `--dangerous` (they mutate
  live terminal/process state).
- **Spawn is two-layer gated (WL-047).** Launching a child agent passes **two**
  independent trust checks, both no-shell argv-only: (1) the **child program**
  (`argv[0]`) must resolve inside weave's trusted directories — the same absolute-
  path trusted-dir set that constrains every mux/`git` binary — so a remote spawn
  cannot launch an arbitrary binary off `$PATH`; and (2) the **cwd** must fall under
  the **spawn allowlist** — `Config::spawn_dir_allowed` canonicalizes the cwd and
  each allow-dir (resolving `..` and symlinks, so a traversal/symlink escape fails)
  and requires a prefix match. The allowlist is `spawn_allowed_dirs` in
  `config.toml`, overlaid by `WEAVE_SPAWN_DIRS` (`split_paths`), and is **empty ⇒
  deny by default**. The MCP/remote surface **hard-denies** a disallowed cwd; the
  operator-local CLI **warns but proceeds** (the operator already has a local
  shell). The child argv is additionally bounded (`MAX_SPAWN_ARGS`,
  `MAX_SPAWN_ARG_LEN`, NUL/control rejected via `spawn_arg_ok`), and the child's
  unguessable identity comes from a parent-minted **birth certificate** threaded as
  `WEAVE_BIRTH_CERT` (§6), so a spawned peer's identity cannot be hijacked. This is
  the Rust-native equivalent of repowire's `daemon.spawn.allowed_paths`
  (`docs/REPOWIRE-PARITY.md` §7).
- **Post-send hooks are no-shell, env-only (WL-036).** A configured
  `[[post_send_hook]]` runs an operator-authored external program on the send/ack
  path, and is a security-invariant surface held to the same no-shell discipline as
  spawn: the `argv` is the **fixed operator-authored vector** from `config.toml` —
  weave **never** substitutes message text into an argv element — and `argv[0]` is
  resolved via `resolve_trusted_program(argv[0])` (the trusted-dir constraint above),
  so a hook cannot launch an arbitrary `$PATH` binary. Message-derived strings reach
  the child **only** as environment values (`Command::envs`:
  `WEAVE_HOOK_{EVENT,SENDER,RECIPIENT,SUBJECT,MESSAGE_ID,PAYLOAD}`); the **body is
  never exported**, and a hostile subject is an inert env value because no shell
  exists on this path. The wait is bounded (a slow hook never hangs send) and every
  failure is logged to stderr only — never propagated, never on the MCP JSON-RPC
  stdout frame. See §SECURITY for the full execution model.
- **Backup extraction is traversal-guarded (WL-035).** `weave restore` runs
  `archive::safe_entry_name` on **every** parsed USTAR entry before using it — a
  closed allow-list (`messages.db`/`config.toml`/`settings.json`/`MANIFEST`) that
  rejects absolute paths, any path separator, `.`/`..`, NUL, and over-long names — so
  a hostile archive cannot write outside the target. The snapshot is a parameterized
  `VACUUM INTO ?1` (the path is **bound**, never inlined), never a raw copy of a live
  WAL DB, and is read-back-verified at both ends.
- **Identity is advisory.** Session names are free strings with no
  authentication — appropriate for a single-user local mesh. weave does not
  defend one local session against another impersonating it; that is out of
  scope for the local-trust model (and would be the job of a future relay tier).
- **Cross-store access is read-only (Tier-1).** Federation (§9) opens foreign
  stores with `SQLITE_OPEN_READ_ONLY` and never writes them, so aggregating
  another project's peers/sessions cannot mutate that project's store and stays
  inside the single-local-trust-domain assumption.
- **Owner-only-writes (Tier-2).** Cross-store delivery (§10) never lets store A
  write store B. A sender deposits a directed *intent* into its **own** outbox; the
  recipient pulls each allowed source **read-only** (`open_readonly`,
  `SQLITE_OPEN_READ_ONLY`, no schema/migrate/harden) and commits the intents
  addressed to it into its **own** inbox. Every write the pull driver performs
  (`Store::send`, `pull_cursor_set`) targets the *local* store — the source is
  never written, migrated, or created. This is a first-class structural invariant
  (the storage engine rejects any write to the read-only handle), proven by a
  byte-unchanged-source test on both backends. It is what keeps "identity is
  advisory" acceptable across stores: store A cannot mutate store B, so the only
  thing cross-store carries is data B chooses to pull and commit itself. This now
  holds **cross-machine**: a remote `libsql`/Turso source (Tier-2 v2, §10) is opened
  read-only too (SELECT-only + write-guard `bail!` + no schema/migrate), so weave
  never writes a remote source. libSQL 0.9.30 has no client-side read-only handle, so
  the recommended deployment contract is a server-enforced read-only Turso token
  (defense-in-depth); weave's own enforcement stands regardless. The remote auth token
  is secret — capped, control-char-rejected, redacted in `Debug`, and never logged,
  injected, or argv'd. The default (sqlite) build refuses remote sources outright with
  a loud stderr note.
- **Signed sender identity is optional (Tier-2, `sign` feature).** By default
  cross-store `from` is advisory, exactly like a same-store send. A `--features
  sign` build (Ed25519, §10) makes a signed `from` unforgeable and **always**
  rejects a tampered or spoofed signature; the default build links no crypto.
- **Human surfaces add an exposed read surface + new secrets (`surfaces` feature,
  WL-048).** The web dashboard is the one surface that renders **stored** text back
  out, so it is an **XSS** target: every Store-derived string (peer names, message
  bodies/subjects, job titles, lease holders, schedule bodies, repo/branch tags)
  passes through the single `weave_core::export::html_escape` (`& < > " '`, reused by
  the dashboard) before it reaches
  the HTML — there is no `format!("…{body}…")` of raw Store text, and a regression
  test asserts an injected `<script>` does not survive unescaped. The dashboard is
  additionally **read-only** (GET only, never mutates), **localhost-bound**, and
  **bearer-gated** (WL-022; a generated token printed to stderr when none is given).
  The Telegram/Slack **bot tokens are new secrets**: config/env only, **Debug-
  redacted**, never logged and never placed in a logged URL or argv (the Bearer
  header / URL path carry them but are never echoed); envctl can inject the token
  env vars. Inbound bot text is bounded (`MAX_BODY`) and the inbound sender is
  sanitized to a valid `check_ident` weave ident before any `Store::send`. The bots
  and dashboard **spawn nothing** (no-shell invariant intact); all logging is stderr.
- **Governed web access is an egress + child-process surface (`obscura` feature,
  WL-049).** Forwarding `browser_*` ops to a spawned obscura makes weave a potential
  confused-deputy / SSRF vector, so the seam is hardened on four axes. **(1) SSRF /
  loopback:** every URL-bearing op runs through `webpolicy::check_url` →
  `host_is_internal`, which default-denies loopback / `localhost` / link-local (incl.
  the cloud-metadata endpoint `169.254.169.254`) / RFC1918 private / `*.local` /
  bare-IP targets unless `obscura_allow_internal=true`. The guard also normalizes the
  **encoded-loopback forms** a browser canonicalizes to the same internal address —
  decimal (`2130706433`), hex (`0x7f000001`, `0x7f.0.0.1`), octal (`017700000001`),
  trailing-dot FQDN (`localhost.`, `127.0.0.1.`), and IPv4-mapped IPv6
  (`::ffff:127.0.0.1`) — so those are explicitly blocked too (any non-DNS-name
  numeric/hex/octal authority fails closed into the bare-IP deny branch). The one
  documented residual is **DNS-rebinding**: a normal-looking public hostname that
  resolves to an internal IP at fetch time — weave validates the URL *host*, not
  obscura's resolved IP, so operators reaching sensitive internal services should also
  network-isolate the obscura host. **(2) Child-process trust:** `obscura` is
  resolved to a **trusted absolute path** (never ambient `$PATH`) and spawned
  **argv-only** (no shell, no built command string), each argv element bounded by
  `spawn_arg_ok`; the child is reaped on `Drop`/`--stop` (no orphans). **(3) Child
  output / secret redaction:** the child's stdout is a pipe weave reads but never
  re-emits on its own (JSON-RPC) stdout, the child's stderr is `null`'d, and the
  obscura proxy URL / auth token are SECRETS (Debug-redacted, passed via env/argv but
  never logged). **(4) Deny-by-default + non-ambient:** no web op runs unless the
  operator explicitly allow-lists it, and `weave_web` is a **dangerous** tool
  (blocked in safe HTTP mode); access is gated/leased/audited like any other mesh
  work. **Residual (documented, not a code gate):** stealth-scraping ToS/legal
  exposure is the operator's responsibility — weave provides governance and audit,
  not a legal shield (ADR-0002 "Residual risk").

---

## 8. Comparison to mcp-broker and repowire

weave is the third iteration of inter-session messaging on this box, built to
keep what worked and drop the operational weight.

| | `mcp-broker` | `repowire` | **weave** |
|---|---|---|---|
| Language / footprint | Python + libSQL (uv venv) | Python (uv tool) | **Rust, one static binary** |
| Push to a running session | ❌ poll-only | ✅ via daemon | ✅ **sender injects directly** |
| tmux injector | n/a | ✅ | ✅ |
| Native zellij injector | n/a | ❌ | ✅ |
| Other muxes | n/a | ❌ | ✅ kitty / wezterm / screen |
| Daemon required | no | **yes** (127.0.0.1:8377) | **no** (optional later) |
| Paste-safe submission | n/a | partial (had cancel bug) | ✅ per-mux idiom |
| MCP-native | ✅ | ✅ | ✅ |
| Tracked ask/answer/ack | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, pure DB** |
| Ask-many (fan to N peers) | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, read-time aggregate** |
| Durable job board (poll/claim) | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, poll-only** |
| Circles (visibility scoping) | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, pure-DB column + caller-side filter** |
| Orchestrator role (per-circle coordinator) | ❌ | ✅ (daemon registry) | ✅ **daemon-free, claim + `is_alive` verdict** |
| Rich presence (turn_state + description) | ❌ | ✅ (daemon registry) | ✅ **daemon-free, hook-auto + read-time TTL** |
| Autonomous dispatch / agent-spawn | ❌ | ✅ (JobRunner daemon) | ⏳ deferred (later runner epic) |
| Storage | libSQL DB | service state | libSQL-compatible SQLite file |
| Cross-machine push | ❌ | ✅ (hosted-relay daemon) | ✅ **consent-based push (opt-in, daemon-free)** — ADR-0005 / §10 |
| Telegram / chat bridges | ❌ | ✅ | ✅ (`--features surfaces`, poll-only) |

- **mcp-broker** (`broker_send/inbox/history/sessions/clear`, libSQL, runs under
  a uv-managed CPython) proved the broker semantics weave adopts — per-reader
  read tracking, `to:"all"` broadcast, history, sessions, clear-with-confirm —
  but is **poll-only**: a running session is never flagged; it sees a message
  only when it next calls `broker_inbox`.
- **repowire** added real push (peer registry + tmux pane injection + Claude
  lifecycle hooks) but at the cost of a Python runtime, a **long-running daemon**,
  and a large product surface (relay, Telegram, dashboard). It is **tmux-first
  with no native zellij injector**, which matters because this box's daily shell
  is zellij.
- **weave** keeps the broker semantics and the push, drops the daemon (the mux
  CLIs reach any pane from any process, so the *sender* injects) and the Python
  runtime, and ships a **native multi-mux injector** (tmux + zellij first-class,
  plus kitty/wezterm/screen) in a single dependency-free binary. Cross-machine
  relay and chat bridges are deliberate non-goals for now; the libSQL-compatible
  on-disk format leaves a clean path to Turso replicas if cross-machine ever
  becomes a real need.
- **weave⊇repowire parity (P1).** repowire's headline advantage over a plain
  mailbox was a **tracked** ask/answer/ack round-trip — but it required the
  long-running daemon to mediate it. weave now closes that gap **daemon-free**: the
  `asks` side-table (§6) layers a correlation-tracked `open → answered → acked`
  lifecycle on the existing append-only `messages` + the caller-side injector, with
  an honest delivery verdict and **no new dependency**. It is local-mesh
  point-to-point in P1; broadcast and cross-store ask are future epics.
- **weave⊇repowire parity (P2 — ask-many).** repowire could also fan one question
  to many peers and collect the replies; weave now matches that **daemon-free** with
  `ask_many` / `ask_many_result` (§6): a small `ask_groups` parent anchor plus the
  additive `asks.parent_id` column turn each target into a normal P1 `ask`, and the
  parent view is computed as a **read-time aggregate** (no background ticker, no stored
  deadline) — `complete | partial | pending` with the totality `answered + acked +
  pending + failed == target_count`. It is **best-effort** (one bad peer is a per-child
  error, not a whole-call failure, matching repowire), **no `store → inject` edge** (the
  per-child nudge is fired caller-side), bounded by `MAX_ASK_MANY_TARGETS = 64`, and
  ships with a **dual-backend additive migration** and **no new dependency**. Explicit
  peer list only (circles compose in a later epic); cross-store fan-out remains future work.
- **weave⊇repowire parity (P3 — job board, poll-only).** repowire's other
  daemon-mediated capability was a **durable job board** (`tracked_work`): persistent
  work rows with a lifecycle, claimed and reported on by workers. weave now matches the
  **poll-only** half **daemon-free** with the additive `jobs` table (§6): durable rows
  driven by the pure `JobState` machine (frozen terminal set; cancel/expire interrupt),
  **claim-by-update with `attempt_id` fencing enforced in the store** (a re-claim fences
  out a stale worker), and **cooperative cancel** (a worker honors the flag on its next
  poll — no daemon needed to request it). Result/error/artifacts are TEXT JSON. It ships
  with a **dual-backend additive migration**, **no new dependency**, and **no `store →
  inject` edge**; the **runner-only columns are excluded** (lease/cron/spawn-exec).
  **Autonomous dispatch is explicitly deferred** — a JobRunner that *acquires and runs* a
  job by spawning an agent, the cron scheduler, and the dispatch-lease ledger belong to a
  later runner epic (Tier B). P3 is **local-mesh poll-only**: workers poll and claim; there
  is no auto-dispatch yet.
- **weave⊇repowire parity (P5 — rich presence).** repowire's peer registry carried a
  live `turn_state` (idle/working/awaiting_input/pending_first_turn) and a self-reported
  `description`, both mediated by its daemon. weave now matches this **daemon-free** with
  three additive `peers` columns (§6): **turn_state** is **auto-set by the lifecycle hooks**
  (session→pending_first_turn, prompt→working, stop→idle, notification→awaiting_input) as a
  best-effort write that never sinks delivery, plus an explicit `set_turn_state` setter;
  **description** is a free-form, control-stripped, 200-char self-set string with a
  **read-time 900 s TTL** (the pure `expire_description` at the store read seam — no sweeper,
  the stored row untouched). Both are **owner-only** (UPDATE bound to the caller's own row),
  surfaced **compactly and non-noisily** (a marker only for a non-idle turn_state or a live
  description, so an unset peer's output is byte-identical to pre-P5). It ships with a
  **dual-backend additive migration**, **no `store → inject` edge**, and **no new dependency**
  (a `#![recursion_limit = "256"]` compile-time attribute was added for the larger MCP tool
  registry — an attribute, not a crate). Local-mesh only.
- **Retirement decision (2026-06).** weave has achieved functional parity with repowire
  (P1–P5) and subsumes mcp-broker's core mailbox semantics. On this box,
  **mcp-broker and repowire are considered retired**. New work and active automation
  should target weave exclusively. Runtime coexistence is preserved only where
  `weave setup` merges hooks rather than clobbering them, so legacy hooks continue to
  function until manually removed.

---

## 9. Read-only multi-store federation (Tier-1)

A session normally sees only its own `WEAVE_DB`. Federation lets `weave peers` /
`weave sessions` (CLI and the matching MCP tools) **aggregate peers and sessions
across several stores read-only**, so an agent can see sessions living in other
projects' mailboxes without those projects sharing one DB.

- **Configuration.** `WEAVE_PEER_DBS` (comma- or path-list-separated) and/or
  `peer_dbs = [...]` in `config.toml` list extra store files. They are unioned,
  validated, deduped, the local store is dropped (no self-federation), and the
  list is capped at `MAX_PEER_DBS` (16). Unset ⇒ empty ⇒ behavior identical to
  a single-store run (the listings are byte-identical).
- **Read-only by construction.** Each foreign store is opened via
  `open_readonly` (`SQLITE_OPEN_READ_ONLY`, no `CREATE`, no schema, no migration,
  no permission hardening) on **both** backends — the storage engine rejects any
  write, so the guarantee is structural, not a convention. libSQL 0.9 exposes the
  same `SQLITE_OPEN_READ_ONLY` open, so neither backend is gated off.
- **Aggregation + dedup.** `federated_peers` / `federated_sessions` open each
  extra store read-only, list it, and feed the rows through the pure
  `merge_peer_views` / `merge_session_views`. Peers dedup on `(name, host)`
  (tie-break: alive > not-alive, then newer `last_seen`, then local origin);
  sessions dedup on `name`, keeping `max(last_activity)` and **never summing
  unread** (a foreign store's unread is not in this session's local inbox — Tier-1
  has no cross-store inbox). Presence reuses §6 `is_alive` unchanged (a foreign
  peer on a different host fails open to TTL).
- **Origin tagging.** Foreign rows are tagged ` (via <store-label>)` in text and
  carry additive `origin` / `foreign` fields in `--json`; local rows are
  unchanged (regression-safe). `doctor` reports configured / ok / skipped store
  counts.
- **Failure isolation.** An unreadable / missing / non-weave / locked extra store
  is **skipped** — a note goes to stderr (MCP keeps stdout clean) and the local
  listing still returns with exit 0. One bad path never breaks the command.

**Tier-1 is read-only aggregation.** It can never deliver a message into your
inbox; `pull_from` (a strictly higher trust grant) does that — see §10. A path may
appear in both lists; adding a store to `peer_dbs` to *view* it never silently
upgrades it into a *delivery* source.

---

## 10. Cross-store delivery (Tier-2)

Tier-2 lets sessions in **different stores** message each other without sharing one
`WEAVE_DB`, using a broker-mediated **request-pull** model (Option C) in which the
DB files are the only shared state and **only a store's owner ever writes it**
(§7 owner-only-writes).

### The flow

1. **Send (owner of A).** `weave send --to-store <B-store> --to <name>` (or
   `weave_send` with `to_store`) writes an `Intent` into **A's own** `outbox`. B's
   store is never opened on the send path. A cross-store broadcast is refused
   (directed delivery only). `weave outbox` / `weave_outbox` inspect A's pending
   intents read-only.
2. **Pull/commit (owner of B).** B lists A among its delivery sources
   (`WEAVE_PULL_FROM` / `pull_from = [...]`, distinct from `peer_dbs`). On each
   drain (the `prompt`/`stop` hook, `weave watch`, the MCP `weave_inbox` drain) or
   an explicit `weave pull`, B opens each allowed source **read-only**, reads the
   intents addressed to it since its per-source cursor, and commits each into its
   **own** inbox via the normal local `Store::send` (so B assigns the id and
   timestamp). It then advances `pull_cursor` for that source.

### Idempotency + the at-least-once contract

The dedup key is the **source's `outbox.id`** (`AUTOINCREMENT`, append-only ⇒
monotonic), recorded per source in `pull_cursor(source, last_id)`; a pull reads
only `id > last_id`. A normal re-drain therefore **never duplicates**. The cursor
is advanced **after each commit** (not one batch transaction — friendlier to the
async libsql path). The only re-delivery window is a crash *between* committing a
message and advancing the cursor, which on the next drain re-delivers **at most one
intent** — a **bounded, single-intent at-least-once** guarantee, not whole-batch
replay. A misaddressed or malformed intent is skipped and the cursor still advances
past it, so one poison row cannot wedge a source. Each drain is bounded to
`MAX_PULL_PER_DRAIN` intents per source (DoS guard).

### Remote sources — cross-machine pull (Tier-2 v2)

A delivery / federation source need not be a local file. A `StoreSource` (defined in
`config`, below `store`/`main` in the DAG) is either `Local(PathBuf)` or
`Remote { url, token }`, classified by URL scheme (`classify_source`:
`libsql://`/`https://`/`wss://` ⇒ remote, else a local path). Source lists split
**comma-first** (`split_source_list`) so a remote URL is kept whole; the platform
`:`/`;` split still applies only to local fragments. The remote auth token comes from
`pull_token` / `WEAVE_PULL_TOKEN`.

- **Read-only enforcement, now cross-machine.** A remote source is opened with
  `LibsqlStore::open_readonly_remote` (`Builder::new_remote(url, token)`), which sets
  `read_only = true`, runs **no schema/migration/hardening**, and creates no local
  file (a pure `new_remote` connection has no path). The foreign handle is touched
  SELECT-only (`list_peers`/`sessions`/`list_outbox`), every write method hard-traps
  via `guard_writable()` (a `bail!`, not a debug-only assert), and commits land in the
  local owned store with a local per-source cursor advance. The owner-only-writes
  invariant (§7) therefore holds across machines, not just across local files.
- **libSQL 0.9.30 has no client-side read-only handle** — read-only for a pure remote
  connection is a server-side (Turso auth-token scope) property only. The recommended
  deployment contract is a **server-enforced read-only token**
  (`turso db tokens create <db> --read-only`), validated by the server regardless of
  client behavior. weave **cannot mint or introspect** that scope; its own SELECT-only
  + write-guard + commit-local enforcement stands independently as defense-in-depth.
- **Default-backend (sqlite) loud rejection.** The default build has no libsql client,
  so its `store` free functions skip every `Remote` source with a loud stderr note
  (`reject_remote_source`, scheme+host only via `remote_scheme_host`) and count it as
  unsupported (`weave doctor` surfaces `federation_remote_unsupported`). Remote
  sources require a `--features libsql` build.
- **Per-source token resolution.** A source-list entry may carry an inline
  `LABEL=<remote-url>` prefix that selects a distinct token from the env var
  `WEAVE_PULL_TOKEN_<LABEL>`. Resolution is **entirely in `config`** — `StoreSource`
  carries no `label` field (`Remote { url, token, timeout_ms }`): a private
  `parse_labeled_source` splits and validates the label (`is_valid_label`: non-empty,
  ≤ `MAX_LABEL_LEN` = 64, charset `[A-Za-z0-9_]`, uppercased), and only treats the
  prefix as a label when the right side classifies as a remote URL — otherwise the
  whole entry is passed verbatim to `classify_source`. `per_source_token` then resolves
  with precedence **per-source `WEAVE_PULL_TOKEN_<LABEL>` (exact `env::var`, no
  `env::vars()` scan) → shared `WEAVE_PULL_TOKEN` / `pull_token` → none**; the
  per-source value goes through the same `sanitize_token` gate and, if rejected,
  **falls through** to the shared token. The label is a *resolution input only* — it
  is consumed to build the env-var name and never travels on `StoreSource`, into a log,
  or adjacent to a token. An unlabelled (or invalid-label) entry resolves identically
  to before, so the change is backward compatible. The label is not a secret (it names
  the env var); the token is, and must never be inlined. Because `peer_db_sources` and
  `pull_from_sources` both call the SAME `resolve_store_sources`, the LABEL namespace
  (and per-source token) covers remotes in **both** `peer_dbs` and `pull_from` — there
  is one resolver, no second token scheme.
- **Token hygiene.** The token is capped at `MAX_TOKEN_LEN` (8192) with control chars
  rejected (`sanitize_token`), redacted to `<redacted>` by the manual `Debug` on
  `StoreSource::Remote` and on `Config`, and reaches **only** `Builder::new_remote` —
  never a log line, never an argv, never interpolated into SQL or a command string.
  This applies equally to per-source and shared tokens. `weave doctor` re-derives the
  resolved tier per remote source (`PullTokenTier` via `peer_db_remote_token_tiers`, a
  token-free enum) and prints only aggregate counts (per-source / shared / none) on a
  `remote tokens:` line — never a token byte and never a label↔token pairing.
- **Network-failure handling.** libSQL exposes no client timeout knob for a remote
  connection, so each remote `block_on` is bounded by `tokio::time::timeout` (the
  `time` tokio sub-feature, gated behind the existing `libsql` feature — the default
  build gains nothing). A connect/query/timeout error is just another **per-source
  skip** (the existing failure-isolation path: note on stderr, continue), and because
  commits land local-only with a per-intent local cursor advance, the bounded
  single-intent at-least-once / one-intent-per-crash guarantee is preserved unchanged.
- **Per-source remote-call timeout.** The timeout that bounds each remote call is
  resolvable per source on the SAME LABEL namespace as the token, via
  `WEAVE_PULL_TIMEOUT_MS_<LABEL>` (precedence **per-source → global
  `WEAVE_PULL_TIMEOUT_MS` → `REMOTE_TIMEOUT_MS_DEFAULT` (5000 ms)**). It resolves in
  `config` (`per_source_timeout`, mirroring `per_source_token`) — values parsed and
  **clamped to `[MIN_TIMEOUT_MS=50, MAX_TIMEOUT_MS=600000]` ms**; a `0`/unparsable/
  out-of-range value falls through to the next tier (the bound is never disabled). The
  resolved value is carried to the store on the new `StoreSource::Remote.timeout_ms`
  field (NOT a secret; shown verbatim in `Debug`) — it does **not** enter
  `source_cursor_key` (two configs differing only in timeout share one cursor). The
  libSQL backend threads it through `open_readonly_remote(url, token, timeout_ms)` and
  stores it on `LibsqlStore.remote_timeout` so `remote_timeout_for(Option<u64>)` bounds
  both the connect and the read SELECTs; `None` ⇒ the global/default fallback (identical
  to before). `REMOTE_TIMEOUT_MS_DEFAULT` is **owned by `config`** as the single source
  of truth and imported by the store, so the config-resolved and store-fallback paths
  cannot drift. `weave doctor` / `weave_doctor` print a token-free `remote timeout:`
  line (per-source / global / default tier counts via `PullTimeoutTier` +
  `peer_db_remote_timeout_tiers`, plus the effective ms range) — never adjacent to a
  token, never a token byte.

### Per-source token/timeout parity across both source kinds

The two per-source knobs — `WEAVE_PULL_TOKEN_<LABEL>` and
`WEAVE_PULL_TIMEOUT_MS_<LABEL>` — hold at **parity** across **both** federation
source kinds (`peer_db` Tier-1 visibility and `pull_from` Tier-2 delivery) along
three axes:

- **RESOLVED.** Both `peer_db_sources` and `pull_from_sources` route through the SAME
  `resolve_store_sources_with_tiers`, so a labelled remote resolves its token AND
  timeout identically regardless of which list it appears in (one shared LABEL
  namespace, one resolver — no fork).
- **APPLIED.** Every foreign remote open — Tier-1 (`federated_peers` /
  `federated_sessions`) and Tier-2 (`pull_from_store`) — funnels through the single
  `open_source_readonly` → `open_readonly_remote(url, token, timeout_ms)` seam. The
  token reaches `Builder::new_remote`; the timeout bounds both connect and SELECTs.
  There is no source kind that resolves a knob but fails to apply it.
- **SURFACED.** `weave doctor` now reports the resolved tiers/counts for **both**
  kinds. The `peer_db` side already rendered (`federation_remote_*`); the
  previously-missing `pull_from` side is closed by adding the symmetric
  `Config::pull_from_remote_token_tiers` accessor (the sibling of
  `peer_db_remote_token_tiers`) so the rollup treats both kinds uniformly.

The single secret-free rollup is `Config::federation_health() -> FederationHealth`,
holding a `FederationKindHealth` per kind (`peer_db`, `pull_from`) with **only**
counts (`total`/`local`/`remote`, the token tiers, the timeout tiers) and an
effective-ms range (`ms_min`/`ms_max`, `None` over zero remotes so an empty set never
renders a misleading `0-0`) — **never** a token byte nor a label↔token pairing. It is
a **read-only aggregation over already-resolved config tiers** (env/config only),
backend-agnostic, computed via the per-kind `federation_kind_health` helper over the
same `resolve_store_sources_with_tiers` the apply path uses. It adds **no new network
probe**: reachability (ok/skipped) for the `peer_db` set stays the already-computed
`store::federation_status`; the `pull_from` side surfaces resolved counts/tiers only
(opening pull sources for health would be a forbidden new network touch). Both the
CLI `weave doctor` and the `weave_doctor` MCP tool consume this ONE method, so the two
surfaces cannot drift; `main` adds the additive `federation_pull_*` JSON keys + a
`pull sources:` / `pull tokens:` / `pull timeout:` human block, and `mcp` mirrors the
same three human lines.

### Consent nudge on a pulled message — DEFAULT ON

When B commits a message from an **allow-listed** source, B also fires the existing
content-free, paste-safe `Nudge::Nudge` (a fixed "check your inbox" ping) into
**B's own** registered pane, by default (`inject_pulled` defaults to `true`). The
body is never in the keystroke; only B's own pane is ever touched (never a foreign
pane); A has no injection path at all. Gating, in order: (1) `inject_pulled` off ⇒
queue-only; (2) the committing source must pass `inject_allowed_from`
(`allow_inject_from` narrows the inject set to a subset of the pull set; unset ⇒
"same as the pull set"); (3) B must have its own registered, injectable, live pane,
else it falls open to queue-only. **Residual risk:** with the default on, any source
on B's pull/allow set can type a capped nudge into B's live pane — accepting
delivery from a source also grants it a live-pane ping. `WEAVE_INJECT_PULLED=false`
disables it; `WEAVE_ALLOW_INJECT_FROM` narrows it.

The nudge is fired **caller-side** (`main::nudge_pulled`, `mcp::nudge_pulled`),
exactly where the live-send nudge already lives — in modules that already depend on
both `store` and `inject`. The pull driver (`pull_from_store`, a `store`-layer free
function) stays inject-free: it only **records** which source paths committed
(`Pulled.committed_sources`) so the caller can gate per source. No new
`store → inject` edge is introduced; the layering DAG is unchanged.

### Signed sender identity — optional `sign` feature

By default the cross-store `from` is advisory **unless a trust set is configured**.
Building with `--features sign` (Ed25519 via `ed25519-dalek`, mirroring the `libsql`
optional-dependency pattern — the **default build links no crypto**) adds verifiable
identity:

- A new low, pure `sign` module (depends only on `config` + std) owns the canonical
  encoding, sign/verify, hex codec, the keypair file, **fingerprints**, and key
  rotation. The private key lives at `~/.config/weave/ed25519.key` (mode `0600`), is
  never logged or printed, and refuses to clobber an existing key.
- The canonical signature covers `(from, to, body)` — **not** `created`/`ts`, which
  is advisory and re-stamped by the receiver on commit, so binding it would be a
  fragile coupling with no integrity gain. Length-prefixed with a
  domain-separation prefix so no field boundary is ambiguous.
- A new `keys(identity, pubkey)` table (always present, plain data, both backends)
  stores peers' public keys. `weave key gen|show|fingerprint|add|list|rotate|revoke`
  (subcommand present only under `--features sign`) manages them.

#### Fingerprints

A **fingerprint** is the SHA-256 of the **raw 32-byte public key**:
`fingerprint_full(pubkey) = hex(SHA256(raw 32 pubkey bytes))` (64 lowercase hex, no
label) is the canonical value trust/revocation match against; `fingerprint(pubkey) =
"SHA256:" + first 16 hex chars` is the **display** form only. Trust and revocation
match on the **full** digest (or a full pubkey hex), so a truncated `SHA256:<16-hex>`
display string never matches — truncation can never cause a mis-trust. The helpers
take the **public** key only and never hash the secret; they return `None` (never
panic) on a malformed/oversized/non-32-byte input.

`sha2` was **already in the `--features sign` dependency tree** (a transitive dep of
`ed25519-dalek`), so declaring it directly under the `sign` feature
(`sha2 = { version = "0.10", optional = true }`, pulled only by `sign`) adds **no new
compiled crate to any graph** — the default and `libsql`-no-sign builds gain nothing.

#### The verification decision table

**Sign on enqueue** (A signs its outbound intent if it has a key); **verify on
commit** in `store::verify_pulled_intent` (B, before its local write), under the
threaded `VerifyPolicy` (tri-state strict override, trust set, revocation list).
B looks up the sender's **registered keys** once via `get_keys` (a lookup error is a
hard drop), then decides. Since #7 the registry is **multi-key** (`identity_keys`): a
signed intent COMMITS IFF the signature verifies against **at least one registered
NON-REVOKED key** for the sender — a revoked key that cryptographically verifies is
*skipped* (R1, absolute revocation), the first non-revoked verifying key is sufficient,
and a signature that verifies against **none** of the registered keys is REJECTED as
before. This is **additive**: with exactly ONE registered key the decision is identical
to the prior single-key model (the table below). The new COMMIT path is legitimate
rotation OVERLAP (old + new key both verify during a window) — something #3's
config-based overlap could only express as trust/strictness, never at the
verification layer (which key may actually verify a message). `is_trusted` /
`is_revoked` match a key's full digest against B's trust/revoked lists;
`trust_configured` = trust set non-empty; an identity is **trusted** if ANY of its
registered keys is in the trust set. The effective strictness for the unsigned /
no-registered-key advisory path is:

```text
if strict_override == Some(true)            => STRICT   (user forced everywhere)
else if strict_override == Some(false)      => ADVISORY (user disabled this path)
else if trust_configured && is_trusted(key) => STRICT   (NEW trust-set default)
else                                        => ADVISORY (current default)
```

Every cell below matches `verify_pulled_intent` exactly (COMMIT = local write,
REJECT = dropped, cursor still advances). Read "the registered key" as "**any
registered non-revoked key**" since #7 — with a single registered key the rows are
unchanged. Two load-bearing rules hold in *every* row: a **present-but-invalid**
signature (verifies against NONE of the registered keys) is ALWAYS rejected, and
**R1** — a signature that verifies ONLY against **revoked** key(s) is rejected
unconditionally (each verifying key's revocation is checked BEFORE acceptance,
before any disable toggle). When a signature verifies against both a revoked and a
non-revoked registered key, the non-revoked match wins (COMMIT) — revocation targets
a specific key, not the identity.

| Sender | Signature | DECISION |
|---|---|---|
| trusted (registered key in trust set) | valid, key not revoked | **COMMIT** (unforgeable, attributed) |
| trusted | present-but-invalid | **REJECT** (always — forgery/tamper) |
| trusted | unsigned | **REJECT** (trusted ⇒ strict-by-default ⇒ must sign) |
| untrusted (trust set configured, sender outside it) | valid | **COMMIT** (advisory — verified, just not pinned) |
| untrusted | unsigned | **COMMIT** (advisory — unsigned operation preserved) |
| any | present-but-invalid | **REJECT** (always) |
| rotation overlap (old + new registered) | valid against either non-revoked key | **COMMIT** (#7 — both keys verify during the window) |
| revoked (verifies ONLY against revoked key(s)) | valid | **REJECT ALWAYS** (R1 — even with strict disabled) |
| no trust set configured | unsigned | **COMMIT** (advisory — UNCHANGED from today) |
| no trust set configured | present-but-invalid | **REJECT** (always) |
| signed but no registered key for sender | present (unverifiable) | advisory path (no fp to trust) ⇒ STRICT only if forced |
| global strict forced (`Some(true)`) | unsigned/unverifiable | **REJECT** (strict everywhere) |
| global strict disabled (`Some(false)`) | unsigned/unverifiable | **COMMIT** (advisory everywhere — but R1 revoked-signed still rejected) |

Two cells went COMMIT→REJECT from the original (pre-Tier-2) model: `trusted+unsigned`
(strict-by-default) and `revoked+valid-sig` (R1). #7 adds exactly one new COMMIT
path — rotation overlap, a sig verifying against a SECOND non-revoked registered key —
and refines R1 to "verifies only against revoked key(s)"; **no row flips
REJECT→COMMIT**, and the single-key model is preserved verbatim. Every no-trust-set
row is byte-for-byte the original behavior; every present-but-invalid row is still
REJECT. Verification reads only B's own `identity_keys` table + B's receiver-local
config; the source is opened read-only (owner-only-writes intact).

#### Rotation & revocation (multi-key registry + receiver-local config)

Trust and revocation lists are **receiver-local config** (no store table); the
**keys** themselves now live in the multi-key `identity_keys` registry (#7). `weave
key add <identity> <pubkey>` **APPENDS** a key (it no longer overwrites), so old + new
coexist for rotation overlap. `weave key rotate` archives the old private key
(`fs::rename` to a `0600` `ed25519.key.<ts>.bak`, never read or printed), generates a
new key, **registers (appends) it without displacing the old one**, and prints **both**
fingerprints plus overlap guidance: keep BOTH keys registered (`weave key add`) so
in-flight messages signed by EITHER key verify during the window, and trust BOTH full
fingerprints in `WEAVE_TRUST`; once peers have the new key, prune the old with `weave
key remove <identity> <old>` and retire it with `weave key revoke <old-full-fp>`.
`weave key remove <identity> <pubkey-or-fingerprint>` deletes one registration (a full
hex pubkey, or a `SHA256:<64-hex>` fingerprint resolved against that identity's
registered set; ambiguous/no match errors). `weave key revoke <fp>` validates the
value and echoes the `WEAVE_REVOKED=` / `revoked = [...]` line to add (it does not
rewrite a managed config); revocation is unconditional (R1). The emitted rotate/revoke
values are the **full** `SHA256:<64-hex>` form so they are actually accepted by
trust/revoke matching. `weave key revoke` additionally writes a best-effort
`declared` row to the `revocations` audit log (provenance only; never a decision
input — see the `revocations` table above). `doctor` reports secret-free per-identity
key counts (`sign_key_identities`, `sign_registered_keys`, `sign_identities_multi_key`),
plus the count of registered keys currently revoked (`sign_registered_keys_revoked`)
and the recorded revocation-event count (`sign_revocation_events`). The MCP
`weave_doctor` tool emits the same sign-gated verify summary at **parity** (strict
mode, trusted/revoked counts, registered-key count, registered-revoked count,
revocation-event count, own fingerprint) — counts + the local fingerprint only,
appended to the JSON-RPC result frame (stdout discipline intact). `weave audit
revocations` lists the log read-only.

`sign` is a low module (`model ← config ← sign`); `store` depends down on it for
verify-on-commit; `main`/`mcp` depend down on both. `VerifyPolicy` lives in `store`
in every build (inert without `sign`) so both backends' free-fn signatures are
identical. No upward edge.

### Cross-machine push (ADR-0005) — the A-initiated dual of the Tier-2 pull

Tier-2 above is **pull-initiated**: A deposits a signed `Intent` in **A's own**
`outbox`, and B's pane lights up only when **B next polls** (`prompt`/`stop` hook,
`weave watch`, `weave pull`). WL-056 adds the missing capability — **latency-free
remote delivery**: a sender A on machine 1 delivers to B on machine 2 and **B's pane
lights up without B polling first** — *without* breaking any non-negotiable.

The key insight: **push is the A-initiated dual of the same pull-commit pipeline**,
not a second delivery path. The receive side is exactly a Tier-2 pull-commit, just
*triggered by A's HTTP request instead of B's poll*:

- **Receive = `weave_push`, a write action on the existing `--features surfaces`
  HTTP surface.** The WL-052a `POST /api` action set (the same bearer-gated surface
  that routes mutating ops through the shared `dispatch_request`) gains a `weave_push`
  op carrying the wire form of an `Intent` (`{from, to, body, sig?, to_host?,
  subject?, idempotency_key?, trace_id?, priority?, ttl?}`). No new socket, no new
  listener, no always-on process. B has a receive path **iff** B runs `weave
  dashboard --write` (opt-in, `--features surfaces`-gated, default OFF).
- **The handler is the Tier-2 commit, verbatim.** `tool_push` parses the body into
  an `Intent`, builds the receiver's `VerifyPolicy` from `Config` exactly as the pull
  path does, and commits via the **existing** `store::commit_pulled(store, me,
  "push:<from>", &policy, vec![intent])` — re-validation, signature verification
  (`verify_pulled_intent` → `sign::verify_intent`), `Store::send` (B assigns id/ts),
  and `idempotency_key` dedup are all inherited unchanged. On `committed == 1` it
  fires the existing caller-side consent nudge (the `nudge_pulled` seam) into **B's
  own** pane. **A never writes B's store; A never touches B's pane** — only *who
  triggers* the commit changes (A's request vs B's poll), never *who performs* it.
- **Send = `weave push --to <name> --host <url:port> [--token …]`**, a CLI verb (plus
  the `weave_push` catalog op reachable through the meta-tool's `call` mode) — **not**
  a standing MCP tool (ADR-0003: zero added standing tokens). It signs the canonical
  `(from,to,body)` if A is keyed (`sign_intent_if_keyed`) and POSTs the Intent to B's
  `/api` with `Authorization: Bearer <token>`, reusing the existing blocking+rustls
  `reqwest` client (no new HTTP dep). `--host` is **EXPLICIT-ONLY** — never
  auto-resolved from message content (SSRF avoidance).
- **Idempotency without a cursor.** Push has no per-source `pull_cursor` high-water
  mark, so dedup rests entirely on the `idempotency_key`. The send path **always**
  populates it (synthesizing `push:<from>:<fnv1a(body)>` when A omits one), so a
  retried POST never double-commits.
- **Bind posture is an explicit operator opt-in.** `serve`/`dashboard` default to
  `--bind 127.0.0.1` (posture unchanged). Cross-machine requires a deliberate routable
  `--bind` (e.g. `0.0.0.0` or a Tailscale address); a non-loopback bind with an
  **empty** bearer token is **refused before the socket opens** (fail-closed — no open
  listener on a routable address). Recommended deployment is a private overlay
  (Tailscale / WireGuard / SSH tunnel).

How each non-negotiable survives: **owner-only-writes** (B's own handler does every
write); **no-daemon-by-default** (no relay/listener on the default path — the default
`cargo build` is byte-identical, `cargo tree` unchanged, zero new deps); **verify-on-
commit** (a forged/unsigned-from-trusted Intent is rejected via the unchanged policy
before any write — bearer gates transport, the ed25519 signature gates identity,
defense in depth); **token-light** (no new standing MCP tool — the standing budget
test is unaffected); **dual-backend + no new crate** (the handler calls only existing
`Store` methods through `commit_pulled`). See `.handoff/decisions/ADR-0005`.
