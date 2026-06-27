# Changelog

## [Unreleased] — docs/test: enforce MCP friction decisions (WL-079)

- Narrowed ADR-0003 and multi-surface docs from broad CLI/MCP parity claims to an
  explicit command-surface decision rule: every top-level CLI command must declare
  its MCP parity decision and read-only status/diagnostic surface.
- Extended the default-build TUI command catalog (`weave tui --json --pane commands`)
  with `mcp_decision` and `status_surface`, including `codex-tools` and the sign-gated `key`/`audit` commands, and made the
  integration gate compare the catalog exactly to `weave --help`.
- Added status/health visibility checks for background/hook surfaces (`daemon`,
  `hook`, and `responder`) so CLI/daemon-first work cannot become invisible to
  MCP-only agents.

## [Unreleased] — feat: expose provider-switch status diagnostics (WL-078)

- Added `weave provider-switch status [--json]` as a read-only CC Switch bridge
  diagnostic surface. It reports DB presence/readability, schema coverage,
  supported vs observed app types, current provider/model per supported app, live
  config agreement, and proxy/failover/health table presence without writing CC
  Switch or host config state.
- Added the same secret-free provider-switch rollup to `weave doctor --json` and
  concise human `doctor` output, so absent `~/.cc-switch/cc-switch.db` is an
  explicit diagnostic state instead of an opaque open failure.
- Added integration coverage for populated CC Switch status and missing-DB
  diagnostics.

## [Unreleased] — docs: accept layered workspace architecture (WL-077)

- Added ADR-0006 accepting the current `weave-core` / `weave-inject` /
  `weave-mcp` / `weave` Cargo workspace as supported architecture. The invariant
  is one dependency-light Rust binary, not one crate.
- Closed/replaced WL-043 so the backlog no longer carries a stale mechanical
  single-crate collapse mandate. Future structural changes require a fresh
  ADR-level reason tied to user-visible simplification or build-risk reduction.
- Synced CLAUDE.md, ARCHITECTURE.md, PRD, and the handoff backlog to the accepted
  workspace decision; no Rust source, Cargo metadata, or dependency graph change.
- Strict-upgraded surfaced verification flakes: `setup --git-hooks` integration
  tests now isolate HOME, and the fake Ollama model-discovery smoke skips cleanly
  when local loopback binds are blocked by the execution sandbox.


## [Unreleased] — chore: add backlog/docs freshness gate (WL-076)

- Added `scripts/docs_freshness_check.py`, a stdlib CI helper that flags PRs
  touching operator-visible CLI/MCP/user-facing paths unless they update
  `CHANGELOG.md` or `.handoff/loop/backlog.md`, or explicitly carry the
  `[no backlog/doc change]` PR marker.
- Wired the helper into CI as the `docs freshness` job and documented local use
  in `docs/TESTING.md`.
- Marked WL-076 complete in the handoff backlog so release-note/backlog drift is
  now guarded instead of relying on reviewer memory.


## [Unreleased] — docs: complete TASK-0001 north-star reconciliation

- Reconciled the canonical PRD with the actually shipped repowire-superset
  surface: WL-047 spawn/kill, WL-048/WL-052a/WL-073 human surfaces, WL-049
  obscura governance, and WL-050..052 token-light MCP are now recorded as
  shipped rather than future gaps.
- Refreshed the ARCHITECTURE WL-052 wording so dashboard writes and bot command
  grammar are not described as still-pending.
- Marked `.handoff/tasks/TASK-0001.task.json` complete with git-kb/source-review
  evidence; no source code or dependency changes.


## [Unreleased] — test: fix free_port TOCTOU dashboard-spawn flake (WL-058)

> **fix(test): race-robust dashboard spawner.** `spawn_dashboard`/`spawn_dashboard_write`
> picked a port via `free_port()` (bind `:0` → read → drop) then a readiness check that
> only proved *something* listened — so under parallel `cargo test` a port-collision loser
> child could exit while the helper connected to the *other* test's server, returning a port
> it didn't own (the `dashboard_readonly_rejects_post` CI flake). Now a shared
> `spawn_dashboard_inner` treats child-exit-before-listening as the collision signal and
> retries on a fresh port (≤8), requiring our child alive AND the port accepting. Test-only.

## [Unreleased]

### Added

- Added WL-075 supply-chain audit reproducibility: `scripts/supply_chain_audit.py`
  validates the local `deny.toml` advisory policy, proves the default dependency
  graph has no `rustls-webpki`, confirms the residual advisory tree is confined
  to optional libsql TLS, and runs (or gives install guidance for) the same
  `cargo-deny check advisories` command used by CI.
- Added WL-074 whole-store mailbox export: `weave export --all` is now the
  explicit privacy boundary for cross-identity HTML export, backed by a bounded
  dual-backend `Store::all_messages()` API while the default/`--for` path remains
  identity-scoped.
- Added WL-073 bot command parity for human surfaces: Slack now reuses the
  structured bot command grammar and shared `dispatch_request` path, and both
  Telegram and Slack support `/send`, `/ask`, `/answer`, and `/reply` behind the
  explicit `WEAVE_BOT_WRITES=1` gate while keeping read commands always available.
- Added WL-072 worker dispatch integration: `weave job dispatch` now auto-claims
  a queued job for a worker, runs a trusted external runner (default
  `flexnetos_runner`) as argv-only, passes `WEAVE_JOB_*` plus optional
  `WEAVE_FXRUN_AGENT`, appends progress, and records completed/failed terminal
  results through the fenced job lifecycle.
- Added WL-071 MCP parity for exact session-id recipients and job delegation:
  `weave_send`, `weave_notify`, and `weave_ask` now accept `sess_<16-hex>` targets
  through the same shared resolver as CLI paths, and new `weave_job_delegate`
  creates an assigned queued job plus durable `JOB_DELEGATED` worker nudge.
- Added WL-069 unsafe-target avoidance: live injection is skipped when multiple peers share the same mux target, with delivery traces recording `not_injectable/ambiguous_target` and verdicts returning `ambiguous_target_queued`.
- Added WL-068 dimensional liveness diagnostics across `peers --json`, `scan --json`, and `doctor --json`: registered/process/pane/reachable/responsive dimensions, last heartbeat/transport/response timestamps, stale reasons, and per-peer diagnostic rows.
- Added `scripts/target_smoke.py`, a generated-artifact smoke matrix that builds and executes `target/debug/weave` and `target/release/weave` directly, emits `target/target-smoke/target-smoke.json`, and can expand to feature-gated artifacts with `--full`.

### Added (CC Switch provider bridge)
- **`weave provider-switch`** (default sqlite build) reads CC Switch's `~/.cc-switch/cc-switch.db` and
  can `list`, show `current`, or `switch` providers for `claude`, `codex`, and
  `gemini` without launching the Tauri app. The switch path applies the provider
  snapshot to the host's live config, updates CC Switch's current-provider marker,
  and preserves weave lifecycle wiring already present (Claude `hooks`/
  `mcpServers`, Codex `notify = ["…", "hook", "wake"]`, Gemini settings
  merged with existing JSON). `models` auto-loads provider current/model-catalog
  entries plus reachable local Ollama `/api/tags`; `switch-model` updates Claude,
  Codex, and Gemini selected models and refreshes live config when the provider is
  current. Includes `--db` for tests/alternate profiles and `--dry-run` for
  validation-only checks.

### Fixed (WL-057 — `weave setup` no longer persists an ephemeral exe path, fixes #107)
- **`weave setup` stopped writing a transient build/worktree binary path into the
  GLOBAL host config.** It used `std::env::current_exe()` verbatim, so when run from
  `…/target/{debug,release}/weave` (a cargo build output) or a `…/.worktrees/…`
  checkout, that path got persisted into `~/.claude/settings.json` (MCP registration
  + hook commands); once the build/worktree was removed, every session errored on
  the dangling path. setup now resolves a STABLE path to persist: a pure
  `resolve_setup_exe` picks (in precedence order) an explicit `--exe`, else the
  running binary when it is non-ephemeral (the default path is byte-identical), else
  `~/.cargo/bin/weave`, else a non-ephemeral `weave` on `$PATH`, else it falls back
  to the running binary with a loud warning recommending `cargo install --path .`
  (or `weave setup --exe <stable-path>`).
- **New `weave setup --exe <PATH>`** to pin the exact binary path written into the
  host config (highest precedence; validated to exist). The `serve`/daemon
  self-spawn path is unaffected (it spawns a child of the same running process and
  persists nothing).

### Added (enforcing PreToolUse approval gate — WL-055)
- **A real PreToolUse hook that actually BLOCKS dangerous tool calls.** weave had
  the approval *primitive* (`weave_ask_permission` / `permission_verdict` + a
  `DANGEROUS_TOOLS` list) but installed no PreToolUse hook, so nothing enforced it.
  New `weave hook pretooluse` drain reads Claude's PreToolUse JSON, and for a
  dangerous tool (`Bash`/`Edit`/`Write`/`MultiEdit`/`NotebookEdit` + weave's
  dangerous MCP tools) raises a blocking approval on the existing ToolPermission
  machinery, emitting `{"hookSpecificOutput":{"permissionDecision":…}}`. **Deny by
  default, fail closed:** `approve` ⇒ `allow`; deny / no-approver / our own short
  timeout ⇒ `deny`. It enforces its OWN timeout (`pretooluse_timeout_secs`, default
  30s, clamped [1,300]) because Claude's hook timeout fails *open*. Malformed stdin
  or a non-dangerous tool ⇒ `defer` (never breaks the session). stdout is pure JSON.
- **Opt-in wiring:** `weave setup --pretooluse` (Claude only) ALSO installs the
  `hooks.PreToolUse` entry (matcher `Bash|Edit|Write`) with the same idempotent,
  never-clobber-foreign, atomic, read-back-verified discipline as the other hooks;
  `weave uninstall` prunes it. Default `weave setup` does NOT install it (so it
  never surprise-blocks). New config keys `pretooluse_approver` /
  `pretooluse_timeout_secs` (+ `WEAVE_PRETOOLUSE_*` env). **No new standing MCP
  tool** (token-light invariant upheld).
### Added (cross-machine PUSH delivery — WL-056 / ADR-0005)
- **Consent-based cross-machine push**: a sender A on machine 1 delivers a message
  to recipient B on machine 2 and **B's pane lights up without B polling** — the
  A-initiated dual of the existing Tier-2 pull, over **one** commit pipeline.
- **Receive** is a new `weave_push` write action on the existing `--features
  surfaces` bearer-gated `POST /api` surface (`weave dashboard --write`). The handler
  (`mcp::tool_push`) commits via the **existing** `store::commit_pulled` — re-validate,
  verify the ed25519 signature under the receiver's `VerifyPolicy`, `Store::send`
  (B assigns id/ts), `idempotency_key` dedup — then fires the existing caller-side
  consent nudge into **B's own** pane. No new `Store` method, no schema change, **no
  new standing MCP tool** (the catalog op only).
- **Send**: `weave push --to <name> --host <url:port> [--token …] [--subject …]
  [--to-host …] [--priority …] [--ttl …] [--idempotency-key …]` (body from `--body`
  or stdin). Signs the canonical `(from,to,body)` if keyed and POSTs the Intent to B's
  `/api` with `Authorization: Bearer <token>` (token from `--token`/`$WEAVE_PUSH_TOKEN`),
  reusing the existing blocking+rustls `reqwest` client (no new dep). `--host` is
  **EXPLICIT-ONLY** (never auto-resolved from message content — SSRF avoidance).
- **Bind opt-in**: `weave serve`/`weave dashboard` gain `--bind <addr>` (default
  `127.0.0.1`, posture unchanged). A non-loopback bind with an **empty** bearer token
  is **refused before the socket opens** (fail-closed — no open listener on a routable
  address).
- Invariants preserved: owner-only-writes (B commits its own row), no-daemon-by-default
  (default `cargo build` byte-identical, `cargo tree` unchanged), verify-on-commit,
  token-light (standing budget test unaffected), dual-backend, no new crate.

### Changed (libsql dependency surface trimmed — WL-044b)
- **`libsql` is now pulled with `default-features = false, features =
  ["core", "remote", "tls"]`** — only what weave uses (`Builder::new_local`
  local file + `Builder::new_remote` remote Turso over HTTPS). weave uses no
  embedded-replica sync, so dropping the default `replication`/`sync` features
  removes their dependency trees — **eliminating the unmaintained `bincode 1.x`
  advisory (RUSTSEC-2025-0141)** and dropping `tonic`/`tonic-web`/`tower-http`/
  `libsql_replication`/etc. **Zero capability or test change** (libsql backend:
  668 passed / 1 ignored, unchanged); the audit surface drops from 6 advisories
  to 5. The remaining 4 `rustls-webpki` vulns + `rustls-pemfile` live in the
  `tls` feature weave needs for remote HTTPS and remain upstream-pinned (libsql
  pins `hyper-rustls 0.25` even on git `main`) — tracked by WL-044b, deny.toml.

### Security (dependency advisory gate — WL-044)
- **Added a `cargo-deny` advisory gate to CI** (`audit` job) + a `deny.toml`
  policy — continuous supply-chain enforcement that did not exist before. The
  **default shippable binary is advisory-clean** (`cargo tree -i rustls-webpki`
  on default features matches nothing). The 4 open `rustls-webpki 0.102.8`
  advisories (RUSTSEC-2026-0098/0099/0049/0104) + 2 unmaintained warnings
  (`bincode`, `rustls-pemfile`) are confined to the **optional `libsql` feature's
  remote-Turso TLS stack** and are **upstream-pinned** (the patched `rustls-webpki
  >=0.103` needs `rustls 0.23`/`hyper-rustls 0.27`, which `libsql` — incl.
  `0.10.0-pre` — does not depend on). They are listed in `deny.toml` with a
  rationale + removal trigger; the gate fails on any advisory **not** explicitly
  listed. Tracked for removal by **WL-044b** (bump the libsql TLS stack once
  upstream adopts rustls 0.23). See `docs/SECURITY.md` §5.

### Added (faithful ask-thread replay on session import — WL-040b)
- **`weave session import` now replays tracked ask threads + ask-many groups
  (WL-040b, completes WL-040 / casr parity).** WL-040 already exported the `asks`
  array but import **skipped** it ("N asks not imported — see WL-040b"); import now
  **replays** each ask faithfully:
  - New dual-backend **`Store::import_ask`** — a deliberate out-of-order materializer
    that inserts an ask row DIRECTLY in its exported `AskState` (open / answered /
    acked), bypassing the create→answer→ack lifecycle (the question/answer message
    rows already exist from the message-import pass). Mirrored in `store.rs` (named
    `params!`) and `store_libsql.rs` (positional `params(vec![...])`, 15-column INSERT
    order pinned to `row_to_ask`). Parameterized SQL only.
  - **Message-id remap (correctness-critical).** Each ask's `question_msg_id` /
    `answer_msg_id` is rewired to the freshly re-minted local message id (resolved
    from `Store::send`, which returns the existing id on a dedup hit). An ask whose
    message is **absent** from the export is **dangling** → skipped+counted (never an
    inserted broken link).
  - **Ask-many groups round-trip too.** The envelope now carries an additive
    `ask_groups` block (read via the new `Store::list_ask_groups`), replayed via the
    new dual-backend `Store::import_ask_group` **before** the child asks so each
    child's `parent_id` is rewired to the replayed group. `target_count` (totality)
    preserved.
  - **`ExportedAsk` gained** `kind` / `options` / `reply_to` / `close_note` /
    `parent_id` (all `#[serde(default)]`, **no `schema_version` bump** — additive). The
    `reply_to` chain pointer is intentionally NULLed on import (it references a
    regenerated source ask id).
  - **Idempotent re-import** (dedup on the remapped `(asker, askee, question_msg_id)`
    triple / minted `parent_id`); `--as` identity remap applied to asker/askee; the
    import summary reports `N ask(s) replayed, M skipped[, K dangling skipped]; G ask
    group(s) replayed, …` (no longer "skipped"). CLI-only — **no new standing MCP
    tool**, no new dependency. Untrusted-input bounded before any write (`check_ident`,
    `MAX_BODY` caps, id-shape, `AskState`/`AskKind` parsed — unknown state rejected),
    re-validated at the store seam. See `docs/FORMAT-session-export.md`.

### Changed
- **README "Status" refreshed to v0.2.0 reality (WL-045).** Replaced the stale
  `v0.1.0 — 38 tests green … injection to be confirmed` block with the current
  state: the four-crate workspace linking one binary, **706 sqlite / 657 libsql**
  tests green under clippy `-D warnings` + fmt, the token-light single-meta-tool
  MCP surface (ADR-0003), live tmux/zellij injection **validated end-to-end**, and
  the default-OFF zero-added-dep optional features (`libsql`/`sign`/`llm`/`surfaces`/`obscura`).

### Added (multi-provider lifecycle hook templates — WL-042)
- **`weave setup --provider <claude|codex|gemini|aider>` (WL-042, `cross_agent_session_resumer`
  / casr parity).** `weave setup` (and `weave uninstall`) now generalize from
  Claude-only to four coding-agent hosts, each wired into its own config file
  Rust-natively. Default `--provider claude` is **unchanged** (byte-for-byte —
  regression-tested). Each provider write uses the SAME discipline as the Claude
  path: idempotent (re-run = no-op / in-place refresh of a stale entry), never
  clobbers foreign content, atomic temp+rename with a one-time `.weave.bak`
  snapshot at `0o600`, and **read-back verified** (re-read + re-parse + assert
  weave's entry landed and every foreign entry survived; a non-NotFound read error
  aborts without writing — never truncates a populated file). Targets and
  mechanisms:
  - **`codex`** → `~/.codex/config.toml`: sets the top-level `notify` argv key
    (`notify = ["<weave>", "hook", "wake"]`) — Codex's documented automation hook,
    mapped to weave's drain (Codex has no per-event granularity). Line-based TOML
    merge, **no `toml` dependency added** to the bin crate; written Rust-natively
    (NOT via the ecc `.codex` sidecar — drift guard).
  - **`gemini`** → `~/.gemini/settings.json`: merges the same Claude-shaped
    `hooks.{event}` block. ⚠ **Caveat:** Gemini CLI's exact lifecycle-hook key is
    **unconfirmed**; weave scaffolds the documented best-known (Claude-compatible)
    shape and prints the caveat. Update the writer if Gemini confirms a different key.
  - **`aider`** → `~/.aider.conf.yml`: appends a minimal hand-templated `weave-hook:`
    YAML stanza, **no `serde_yaml`/YAML dependency added** (manual compose +
    read-back line-presence check). ⚠ **Caveat:** Aider has no rich lifecycle-hook
    surface; this is a best-effort scaffold that may be ignored until Aider grows
    one. Tracked as a gap in `docs/MULTI-SURFACE-PARITY.md`.
  CLI-only admin capability — **no new MCP tool, no new standing token**, no
  `Store`/schema change.

## [Unreleased] — fix: honest `weave kill` result (WL-047 follow-up)

### Security
- **Read-back verification for destructive config/hook writes (WL-041, casr "verify
  before declaring success" parity).** `weave setup`, `weave uninstall`,
  `weave setup --git-hooks`, and `weave restore` now **re-open and re-parse** every
  config/hook file they rewrite and confirm the intended content actually landed
  before reporting success — never trusting the write blindly (mirrors the WL-035
  backup archive read-back). Concretely: `setup` asserts weave's four lifecycle
  hooks are present AND every pre-existing foreign hook (rtk, repowire, …) survived;
  `uninstall` asserts no weave hook remains AND foreign hooks survived; the git
  pre-commit install asserts the guard line landed, the shebang is present on a
  freshly created hook, and any pre-existing foreign content was preserved (append
  only); `restore` asserts the restored `config.toml` / `settings.json` bytes match
  the archived payload and that settings.json re-parses as a JSON object. A write
  whose re-read does not contain the intended weave entries — or that lost a
  pre-existing foreign hook — now **fails loudly with a descriptive error** (naming
  the `.bak`/`settings.json.weave.bak` recovery path) instead of silently
  succeeding. CLI-only behavior — no new MCP tool, no new standing token.

### Added
- **Canonical session export/import (WL-040), `weave session export` / `weave session
  import` (CLI):** a portable, schema-versioned JSON interchange format for resuming a
  session — its **messages** and its **mesh memory** — across distinct weave instances
  whose row ids and minted correlation ids deliberately do not match (`cross_agent_session_resumer`
  / casr parity). Distinct from the two prior "export" surfaces: **WL-034** `weave export`
  is HTML *presentation*, **WL-035** `weave backup` is a byte-exact host-local DB *snapshot*;
  **WL-040** is the *logical, portable, versioned* interchange. Export reads messages via
  `Store::history`, asks via `list_asks` (recorded for fidelity), and the filesystem mesh
  memory; the document is written atomically (sibling-temp + rename) and read-back-verified.
  Import re-inserts messages via the existing `Store::send` — minting fresh local ids (free
  id-remap) and deduping on `idempotency_key` (a deterministic synthetic key
  `wl040:<identity>:<id>` is used for keyless legacy messages) so **re-import is idempotent**
  — and writes memory via `memory_write`; identity is remapped via `--as` (occurrences of
  the source identity become the importing identity; third-party names preserved). `--dry-run`
  reports counts without writing. The import file is treated as **untrusted external input**:
  every field is bounded (`check_ident`, `MAX_BODY`, subject cap, idempotency/trace shape)
  before any store write; all writes go through the parameterized `Store::send`; no shell, no
  embedded path fields, `--in`/`--out` are UTF-8- and traversal-guarded. **No new `Store`
  method, no schema change, no new standing MCP tool** (CLI is the zero-standing-cost path).
  Asks are recorded in the envelope but **not replayed** in this version (faithful ask-state
  replay needs a new dual-backend `Store::import_ask` accepting out-of-order `AskState` —
  tracked as **WL-040b**); peers are excluded by design (host/mux/birth-cert-local; takeover
  hazard). See `docs/FORMAT-session-export.md`.
- **Idle notification dedup (WL-039), `weave notify --dedup-idle` (CLI) and a `dedupIdle`
  property on `weave_notify` (MCP, zero standing-token cost):** a new idle/notification
  "still waiting" ping from a sender auto-supersedes that sender's prior **unread** idle
  ping(s) to the **same recipient**, so idempotent "are you there?" pings collapse to just
  the latest instead of piling up N-deep. Reuses the WL-037 `superseded_by` spine: the
  superseded predecessors drop out of every unread/peek/inbox/nudge surface while staying
  (flagged) in history/thread/search — no new hide mechanism. **Opt-in** at the call site
  (`--dedup-idle` / `dedupIdle:true`); plain `weave send` / `weave_send` are never touched.
  **Hard safety boundary — dedup can NEVER touch a real message or another session's
  pings:** the supersede `UPDATE` is scoped to rows with `kind = 'idle'` (set only on the
  notify dedup path), `sender = caller` (self-only authz, same spine as `supersede`),
  `recipient` match, still-unread (same definition as `unread_count`), `superseded_by IS
  NULL`, and `id <> new_id` (an idempotency-key replay is a clean no-op). Additive nullable
  `messages.kind` column (NULL/`'normal'` == ordinary message), mirrored across both
  backends with a guarded `ALTER TABLE ADD COLUMN` migration. New
  `Store::supersede_prior_idle(sender, recipient, new_id) -> usize`. **No new standing MCP
  tool** (`dedupIdle` lives in the `weave_notify` catalog op).
- **Ephemeral messages with TTL (WL-038), `weave send --ttl <secs>` (CLI) and a `ttl`
  property on `weave_send` (MCP, zero standing-token cost):** a sender marks a message
  ephemeral by attaching a TTL in seconds; weave stamps a nullable absolute deadline
  `messages.expires_at = ts + ttl`. Before the deadline the message behaves exactly like
  a normal message; at/after it the row is **deleted** (delete-on-sweep — an expired
  ephemeral message is not reconstructable) and excluded from every read surface
  (inbox, unread count, nudge/peek, history, search, inbox_since). Expiry is enforced two
  ways: folded into the existing `gc()` retention pass **and** a new
  `sweep_expired_messages()` called opportunistically before unread/read surfaces (so
  expiry holds with no explicit gc). TTL is capped at `MAX_MSG_TTL_SECS = 86400` (24h),
  validated at both the CLI and MCP seams (`ttl_valid`). Additive nullable
  `messages.expires_at` column, mirrored across both backends; carries through the
  cross-store intent/outbox→pull path via `outbox.ttl` (re-stamped to an absolute
  deadline on commit). **No new standing MCP tool.**
- **Mailbox backup/restore (WL-035), `weave backup --out <path> [--force]` /
  `weave restore --in <path> [--force]`:** a portable **dependency-free uncompressed
  USTAR** archive of a consistent SQLite snapshot (`VACUUM INTO`, never a raw live-DB
  copy) + `config.toml` + the installed Claude `settings.json` hooks + a `MANIFEST`.
  Read-back-verified at both ends; **traversal-guarded** extraction (closed allow-list
  `safe_entry_name`); restore gates DB/config and `settings.json` (with a `.bak`)
  overwrite behind `--force`; remote libSQL is unsupported (no local file to snapshot).
  Re-run `weave setup` after restore to re-register the MCP server. Also: `weave export`
  write now reports its path on failure (GAP-2).
- **Message supersede / successor chains (WL-037), `weave send --supersedes <id>` (CLI)
  and a `supersedes` property on `weave_send` (MCP, zero standing-token cost):** a sender
  replaces a prior message; the predecessor is stamped `superseded_by` and **hidden from
  the recipient's unread inbox** (kept and flagged in history/thread/search for audit).
  Chains supported (only the tail is unread). **Sender-only authorization** — you may
  only supersede your own messages (censorship/DoS guard). Additive nullable
  `messages.superseded_by` column, mirrored across both backends; distinct from
  `in_reply_to` threading.
- **Post-send hooks (WL-036), config `[[post_send_hook]]`:** run an operator-authored
  external program after a matching send/ack. **argv-only, no-shell** spawn (`argv[0]`
  constrained to a trusted dir); message fields reach the child ONLY as `WEAVE_HOOK_*`
  env vars (the message **body is never exported**); recipient matching supports `*`
  (universal) + exact + `BROADCAST` aliases. Fault-isolated and bounded (a slow/failing/
  missing hook never breaks send; failures log to stderr); fired from CLI `weave send`/
  `notify`/`ack` and MCP `weave_send`/`weave_notify`/`weave_ack` via one shared helper.
  **No new standing MCP tool.**
- **Static mailbox export (WL-034/WL-074), `weave export --out <path> [--for <id>] [--all] [--limit N]`:**
  renders a **self-contained, offline, XSS-safe portable HTML** bundle of the caller's
  mailbox with **client-side search** (mcp_agent_mail parity). The file double-click-opens
  with no network — no external script/style/CDN — and is safe with JavaScript disabled (a
  static `<noscript>` table escapes every field). Message data is embedded once in a
  `<script type="application/json">` block whose `</`/`<!--` byte sequences are neutralized
  so a hostile body can't break out, and the client renders via `textContent`/`createElement`
  (never `innerHTML`). Default and `--for <id>` remain identity-scoped; explicit
  `--all` is the privacy decision to include every non-expired local-store message through
  bounded dual-backend `Store::all_messages()`; `--limit N` caps the rows. The pure
  renderer (`render_mailbox_html`) lives in `weave-core/src/export.rs`,
  which also now owns the single `html_escape` source of truth the dashboard reuses.
- **Bot command grammar (WL-052b/WL-073), `--features surfaces`:** the Telegram and
  Slack bridges answer structured `/`-commands — `/inbox`, `/peers`, `/sessions`,
  `/help`, plus gated `/send`, `/ask`, `/answer`, and `/reply` — by dispatching
  through the **same** `dispatch_request` → `call_tool` handler as MCP/CLI (the
  one-handler-many-surfaces law), formatting the result back to the chat. Ordinary
  (non-`/`) text still relays into the mesh as before. Read commands are always
  available; write commands require explicit `WEAVE_BOT_WRITES=1`, then pass the
  same dangerous-tool gate as HTTP write mode. The parser, RPC mapping, and reply
  formatter are pure + unit-tested and shared by both bot bridges.
- **Dashboard write surface (WL-052a), behind `weave dashboard --write` (default
  read-only):** a bearer-gated `POST /api` action route that accepts a JSON-RPC body
  and dispatches through the **same** `dispatch_request` → `call_tool` handler the MCP
  and CLI surfaces use — no parallel write path, so every invariant (input caps,
  parameterized SQL, destructive-op gating, real nudge-inject) is inherited, not
  re-implemented (the WL-052 "one handler, many surfaces" design law). A send issued
  via the dashboard behaves byte-for-byte like a CLI send. Read-only stays the safe
  default (POST → 403 without `--write`). Verified end-to-end through the real binary
  (send → read-back via the API) on sqlite + libsql.
- **`token-light` is now a first-class invariant with a CI-enforced budget gate
  (WL-051, ADR-0003).** The standing MCP `tools/list` surface is budget-capped by
  `MAX_STANDING_TOOLS_BYTES` (≈2k tokens) and guarded by the
  `standing_mcp_surface_is_within_token_budget` test — *adding a capability must not
  add standing tokens*. A regression to the eager-flat table (~180 KB) or a pile of
  standing dispatcher tools trips it immediately. Documented as a non-negotiable
  invariant in `CLAUDE.md` (peer of `dependency-light`); the eager-flat opt-in
  (`WEAVE_MCP_EAGER=1`) is exempt, and full CLI parity is the zero-standing-cost path.
- **Token-light MCP via progressive disclosure (WL-050, ADR-0003).** The standing
  `tools/list` surface is now a single `weave` **meta-tool** instead of 70+ eager
  flat tools, cutting the standing context cost from tens of thousands of tokens to
  ≈ a few hundred — **with zero capability loss**. Every operation is reached on
  demand via the meta-tool's modes: `search {query}` (find ops), `list` (enumerate),
  `describe {name}` (one op's schema), `call {name, arguments}` (invoke it; op names
  may omit the `weave_` prefix). `call` routes back through the same dispatcher, so it
  preserves every guard — the safe-HTTP destructive-op gate is re-applied to the inner
  op and self-recursion is refused. A backward-compatible **eager-flat** mode
  (`WEAVE_MCP_EAGER=1`) restores the full flat `tools/list` for harnesses that need it.
  Pure `weave-mcp` refactor: no `Store`/schema/dependency change; default `cargo tree`
  byte-identical; both backends green.
- **Multi-surface parity matrix (WL-052, ADR-0003).** `docs/MULTI-SURFACE-PARITY.md`
  maps every capability domain onto each surface (CLI / MCP / dashboard / bots) with a
  per-cell verdict, making "full multi-surface parity" **measurable** rather than asserted.
  Result: **CLI and MCP are at full parity** (the agent-facing surfaces); the dashboard
  (read-only) and bots (relay) are the WL-048 v1 baseline. The remaining human-surface
  write-parity is decomposed into tracked cards **WL-052a** (dashboard write/read views)
  and **WL-052b** (bot command grammar), under a single design law: a human surface must
  route to the *same* capability handler as CLI/MCP, never a parallel implementation.
- **obscura governed web access (WL-049, ADR-0002), behind `--features obscura`
  (default OFF):** weave becomes the governance plane for obscura's stealth web
  access. It spawns the separate `obscura mcp` binary (argv-only, trusted-path
  resolved, no shell) and speaks newline-delimited JSON-RPC over its stdio as a
  hand-rolled MCP **client** — **no V8/tokio/obscura crate linked**, and the
  default `cargo tree` is byte-identical (zero new deps; std + serde_json only).
  One token-light `weave_web {action,args,describe?}` MCP dispatcher + a
  `weave web <op>` CLI proxy all 35 `browser_*` ops (ADR-0003), not 35 eager
  tools; `weave_web` is gated as dangerous. Access is **deny-by-default** via a
  pure `weave-core::webpolicy` allow-policy with an **SSRF/loopback URL
  validator** (internal/localhost/link-local/RFC1918/`*.local`/bare-IP denied
  unless `obscura_allow_internal`). Governance reuses the existing
  permission/lease/job Store methods (no new Store method, no schema change). The
  obscura child is lazily spawned, reused, and reaped on `Drop` / `weave web
  --stop` (no zombies); its stderr and any proxy/token secrets are never logged.

### Fixed
- **inject (WL-053):** capture the tmux server **socket** at registration and thread
  it as `tmux -S <socket>` through every command — inject (`send-keys`), spawn
  (`split-window`/`new-window`), kill (`kill-pane`), and liveness (`has-session`). A
  peer registered from a non-default tmux server (`tmux -L <label>` / `-S <path>`, or
  an MCP/CLI process in another session) was previously reachable only via the acting
  process's ambient `$TMUX`, so commands silently hit `/tmp/tmux-1000/default` — the
  wrong/empty server. The socket is parsed from `$TMUX` (`<socket>,<pid>,<session>`)
  and persisted on the existing `peers.socket` column (no schema change); a socket-less
  peer keeps the historical default-server argv byte-for-byte. (Surfaced by the WL-047
  `/verify` live test; complements the honest-kill fix below.)
- **inject/cli/mcp:** `weave kill` (and `weave_kill_peer`) no longer falsely
  report `killed …` when the mux `kill-pane`/`kill` command actually failed
  (non-zero exit — e.g. the pane/session is already gone or the mux server is
  unreachable, such as a non-default tmux socket). `inject::kill` now honors the
  runner's exit status (`Ok(false)` on a non-zero mux exit, mirroring `spawn`,
  which already did); the CLI and MCP report *"could not confirm kill … the
  pane/session may already be gone or unreachable"* instead of a false success.
  Found by `/verify` driving a real tmux server; the prior fake-mux test always
  `exit 0`, so the path was uncovered — a failing-mux regression test was added.

> **What weave is (v0.2.0).** weave is the **Rust-native superset of repowire** —
> a full agent-to-agent **orchestration mesh** in one dependency-light static
> binary, Python-free, no daemon (the DB is the broker): **70 `weave_*` MCP
> tools** with full CLI parity, dual `Store` backends (**sqlite** default +
> **libSQL/Turso**), optional `sign` (ed25519) and `llm` (summarization)
> features, a five-mux native paste-safe injector (tmux/zellij/kitty/wezterm/
> screen) + iTerm2, and a CI gate of six required checks across both backends
> (≈531 sqlite / ≈491 libsql tests). The per-feature `[Unreleased]` blocks below
> are the running log; this header is the reconciled summary so the entries are
> read as the shipped mesh, not the v0.1.0 seed at the foot of this file.
>
> **Shipped orchestration surface (WL-001..033, all merged; ground truth:
> `.handoff/loop/backlog.md`):** workspace split (WL-001); presence daemon +
> MCP tools (WL-002); zellij pane targeting (WL-003); daemon lifecycle tests +
> hardened loop + `weave setup` + tmux bracketed-paste (WL-004..007); live
> injection validation (WL-008/009); mcp-broker/repowire retirement decision
> (WL-010); kitty/wezterm/screen adapters (WL-012); config file (WL-013);
> reminder injection + structured question types (WL-014/015); scheduler
> (WL-016); mesh agent memory (WL-017); birth-certificate identity (WL-018);
> co-orchestrator (WL-019); GitHub review queue (WL-020); PreToolUse permission
> gating (WL-021); streamable-HTTP MCP transport (WL-022); iTerm2 injector
> (WL-023); reservation leases + TTL sweep + pre-commit guard (WL-024/029/030);
> stop-boundary wake (WL-025); idempotency + trace IDs (WL-026); broadcast
> notify/ask (WL-027); FTS5 search (WL-028); message priority + contact policies
> (WL-031/032); LLM thread summarization (WL-033); plus the bonus
> FrankenNetworkX graph extraction. Cross-store federation + Tier-2 pull and the
> ed25519 `sign` path are logged in their own blocks below.
>
> **Mission gaps (in scope, not shipped):** repowire parity audit (WL-046),
> agent spawn/kill (WL-047), Rust-native human surfaces (WL-048,
> dashboard/Telegram/Slack, no Next.js/Python), governed obscura web access
> (WL-049 / ADR-0002, no V8 in core), and the token-light progressive-disclosure
> MCP refactor + budget invariant + multi-surface parity (WL-050..052 / ADR-0003).
> Structural: WL-077 / ADR-0006 accepts the four-crate workspace as the supported architecture; WL-043's mechanical single-crate collapse mandate is closed/replaced.

## [Unreleased] — Rust-native human surfaces (WL-048)

> **feat(mcp/cli/config): read-only web dashboard + Telegram/Slack bridges behind
> `--features surfaces` (WL-048 / ADR-0004).** weave regains repowire's three human
> surfaces — a live **read-only web dashboard** (sessions/presence, recent messages,
> jobs, leases, schedules) served as server-rendered HTML + SSE over the EXISTING
> hand-rolled `std::net` HTTP transport, and **Telegram** + **Slack** bridges (poll-
> only v1) — all **Rust-native, no Next.js, no Python, no async runtime**, behind a
> single new `--features surfaces` Cargo feature (**default OFF** ⇒ the default build
> gains **zero** compiled deps). The bots reuse the **same** optional
> `reqwest` blocking+rustls client `llm` already carries (Cargo unions the feature ⇒
> **one** reqwest copy; `cargo tree` shows none in the default build). The dashboard
> is read-only, localhost-bound, bearer-gated (WL-022), and **HTML-escapes every
> Store-derived string** (the central XSS defense). Surfaces are **CLI subcommands,
> not MCP tools** (ADR-0003 token-light preserved). Closes the last repowire-parity
> gap (`docs/REPOWIRE-PARITY.md` §6).

### Added
- **mcp (`weave-mcp`):** new pure `dashboard` module — `html_escape`,
  `render_dashboard(snapshot, now, host)`, `sse_event`/`sse_keepalive`, and a
  `route(method, path)` classifier (socket-free, unit-tested incl. an XSS-escape
  regression). `http.rs` gains a `serve_dashboard(port, token, store_factory)`
  entrypoint that spawns a **short-lived `std::thread` per connection** (each opens
  its own read-only `Store` handle — `Store: Send` not `Sync`) so a long-lived SSE
  stream cannot starve the MCP port; `handle_connection` additionally answers
  `GET /` (HTML) and `GET /events` (SSE) under the feature, with the **POST/JSON-RPC
  path byte-identical**.
- **cli (`weave`):** `weave dashboard [--port 8788] [--token]` (random token printed
  to **stderr** when omitted), `weave telegram`, `weave slack` subcommands (all
  `#[cfg(feature = "surfaces")]`).
- **bots (`weave`):** `telegram.rs` / `slack.rs` — pure payload-builders
  (`telegram_send_payload` / `slack_post_payload`) and inbound-parsers
  (`parse_telegram_update` / `parse_slack_message`, unit-tested incl.
  missing-field/oversized-body), plus blocking poll loops (`getUpdates` long-poll /
  `conversations.history` poll → `Store::send`; bridge-inbox poll → `sendMessage` /
  `chat.postMessage`). Inbound idents are sanitized + `check_ident`-validated and
  bodies capped at `MAX_BODY` before the store write.
- **config (`weave-core`):** `telegram_token` / `slack_token` (SECRETS — Debug-
  redacted, never logged), `telegram_chat_id` / `slack_channel` / `bridge_identity`
  config keys + `WEAVE_TELEGRAM_TOKEN` / `WEAVE_TELEGRAM_CHAT_ID` /
  `WEAVE_SLACK_TOKEN` / `WEAVE_SLACK_CHANNEL` / `WEAVE_BRIDGE_IDENTITY` env overlays
  (envctl can inject the secrets).
- **features:** `surfaces` added to `weave-core` (`["dep:reqwest"]`), `weave-mcp`
  (`["weave-core/surfaces"]`), and `weave` (`["weave-core/surfaces",
  "weave-mcp/surfaces", "dep:reqwest"]`) — mirroring the `sign`/`libsql` propagation.
  No `Store` change; the feature compiles + serves on **both** backends.

## [Unreleased] — agent spawn/kill (WL-047)

> **feat(inject/mcp/cli): agent spawn/kill (`weave_spawn_peer` /
> `weave_kill_peer`) — argv-only, per-mux, birth-cert identity, spawn allowlist
> (WL-047).** weave can now **launch** a new agent into a fresh mux pane/window and
> **kill** a registered peer's pane/session, **argv-only — no shell, ever** — across
> tmux/zellij/kitty/wezterm/screen. The parent mints a WL-018 birth certificate and
> threads the spawned peer's identity (`WEAVE_SESSION`) + cert
> (`WEAVE_BIRTH_CERT`) into the child's environment, so the child self-registers an
> unguessable identity on its first `weave hook session`. Spawn is **two-layer
> gated**: the child program (`argv[0]`) must resolve inside weave's trusted
> directories, and the cwd must fall under a **spawn allowlist** (deny-by-default
> for the MCP/remote surface, warn-but-proceed for the operator-local CLI). Muxes
> that cannot echo a target id (zellij/screen) are **fail-open** and lean on child
> self-registration; iterm2/none report "unsupported". Closes repowire-parity gap
> #2 (`docs/REPOWIRE-PARITY.md` §2).

### Added
- **inject (`weave-inject`):** pure exact-argv builders `spawn_commands` /
  `kill_commands` (per-mux: tmux/zellij/kitty/wezterm/screen, iterm2/none
  fail-open) and the `spawn` / `kill` runners (trusted-path execution, child-argv
  validation via `spawn_arg_ok`, `WEAVE_SESSION`/`WEAVE_BIRTH_CERT`/`WEAVE_CIRCLE`
  threaded via `Command::envs`, captured target id where the mux echoes one);
  `Injector` trait `spawn`/`kill` default methods; `spawn_arg_ok`, `SpawnOutcome`,
  `MAX_SPAWN_ARGS`, `MAX_SPAWN_ARG_LEN`.
- **mcp (`weave-mcp`):** `weave_spawn_peer` `{ name, cmd:[…argv], cwd?, mux?,
  window?, circle? }` and `weave_kill_peer` `{ name }` tools (+ JSON schemas); both
  added to `DANGEROUS_TOOLS` (disabled on the safe HTTP surface unless
  `--dangerous`). Spawn hard-denies a cwd outside the allowlist; kill errors on an
  unknown peer and reports gracefully on an unsupported mux.
- **cli (`weave`):** `weave spawn <name> --cmd <argv…> [--cwd] [--mux] [--window]`
  and `weave kill <name>` subcommands (`--cmd` uses `allow_hyphen_values` so a child
  argv beginning with `-` is content). CLI spawn warns-but-proceeds when no
  allowlist is configured; hard-denies when an allowlist is set and the cwd is
  outside it.
- **config (`weave-core`):** `spawn_allowed_dirs` config key + `WEAVE_SPAWN_DIRS`
  env overlay (`split_paths`) + `Config::spawn_dir_allowed` (canonicalizing
  prefix-check; deny-by-default; resists `..`/symlink escapes); documented in the
  generated `config.toml` template. Redacted in the `Debug` impl.
- **store (`weave-core`):** additive, backward-compatible change to
  `register_peer_full` in **both** backends (sqlite + libSQL) — the new-peer INSERT
  now honors a supplied `birth_cert` (else mints, exactly as before for `None`), so
  the parent's minted cert can be pre-bound into a freshly spawned peer row. No new
  `Store` method, no schema column, no migration; every pre-WL-047 caller passes
  `None`, so existing behavior is byte-identical.


## [Unreleased] — Codex forge loop (`weave harness forge-loop`)

> **Rust-native Codex task-execution front door.** `weave harness forge-loop`
> dry-runs (default) or delegates one cohesive cycle to `codex exec`, using the
> repo-local `.agents/skills/forge-loop/SKILL.md` as the durable workflow source
> of truth. `weave codex-tools doctor/install` checks and installs the optional
> user-level `/forge-loop` prompt shim without making repo config the source of
> secrets or user-level Codex settings.

### Added
- **cli (`weave`):** `weave harness forge-loop` with task/worktree/budget/execute/safe/json and Codex command/model overrides.
- **cli (`weave`):** `weave codex-tools doctor` and `weave codex-tools install` for Codex forge-loop integration.
- **codex sidecars:** forge-loop skill plus implementer/verifier/guardian agent configs for Codex subagent workflows.

## [Unreleased] — Codex 7-layer harness (`weave harness ide-merge-ide`)

> **Autonomous orchestration surface in the binary.** `weave harness
> ide-merge-ide` dry-runs (default) or `--execute`s the checked-in seven-layer
> Ralph loop: Kimi Code preflight/review around an Ollama-launched Claude
> MiniMax implementation pass, with durable `_workspace` sentinels for
> resume/handoff. The script is spawned argv-only (`bash <script>`, no shell
> string); dry-run prints the layers + exact `WEAVE_*` environment, and
> `--json` emits it machine-readably.

### Added
- **cli (`weave`):** `weave harness ide-merge-ide` subcommand with worktree /
  budget / max-iters / sleep / execute / safe / json flags plus agent-model and
  Kimi Code overrides.
- **harness (`weave`):** `weave/src/harness.rs` builds the plan and runs the
  loop script; dry-run integration test in `weave/tests/integration.rs`.

## [Unreleased] — thread summarization via LLM (WL-033)

> **LLM-powered thread summarization.** Generate concise summaries of message
> threads using an OpenAI-compatible chat-completion endpoint. Summaries are
> cached in-store and can be refreshed on demand.

### Added
- **model:** `Summary` struct with `root_id`, `text`, `model`, `created_ts`,
  `refreshed_ts`; new `summaries` table in both backends with additive migration.
- **store (both backends):** `Store::store_summary`, `Store::get_summary`,
  `Store::delete_summary`.
- **config:** `llm_endpoint`, `llm_api_key` (redacted in Debug), `llm_model`,
  `llm_timeout_secs`, `llm_max_input_chars`; all overlayable via env vars.
- **CLI:** `weave thread --root <id> --summarize [--refresh]`; new
  `weave summarize --text "..."` command.
- **MCP:** `weave_thread_summarize` and `weave_summarize_text` tools
  (feature-gated behind `llm`; return graceful errors when unconfigured).
- **LLM client:** new `weave-core/src/llm.rs` module using `reqwest::blocking`,
  gated behind the `llm` Cargo feature (off by default). Caps input text and
  `max_tokens` for safety.
- **Tests:** store round-trip tests for both backends; unconfigured-endpoint and
  secret-redaction tests for the LLM module.

## [Unreleased] — message priority & contact policies (WL-031 + WL-032)

> **Message importance levels and per-peer contact policies.** Senders can tag
> messages with `low`/`normal`/`high`/`urgent` priority. Recipients can set a
> contact policy (`open`, `auto`, `contacts_only`, `block_all`) on each peer row.
> Priority propagates through cross-store intents so pulled messages retain it.

### Added
- **model:** `MessagePriority` enum (`Low`/`Normal`/`High`/`Urgent`) and
  `ContactPolicy` enum (`Open`/`Auto`/`ContactsOnly`/`BlockAll`); `priority`
  column on `Message`/`Intent` with serde defaults for backward compatibility.
- **store (both backends):** `priority` column on `messages` and `outbox`,
  `contact_policy` column on `peers`, all via guarded additive migration;
  `Store::set_message_priority`, `Store::set_peer_policy`, `Store::get_peer_policy`.
- **CLI:** `--priority` on `weave send`, `weave notify`, and
  `weave broadcast-notify`; new `weave peer-policy --name <peer> [--policy <policy>]`
  command (omit `--policy` to read).
- **MCP:** `priority` optional parameter on `weave_send`, `weave_notify`, and
  `weave_broadcast_notify`; three new tools: `weave_set_message_priority`,
  `weave_set_peer_policy`, `weave_get_peer_policy`.
- **Tier-2:** `Intent` carries `priority`; `enqueue_intent` writes it to the
  outbox and `commit_pulled` applies it to the receiver's local message.
- **Tests:** CLI + MCP roundtrip tests for send/notify priority, broadcast
  notify priority, set-message-priority, peer-policy set/get, and tools/list
  presence.

### Fixed
- libsql `inbox` unread SELECT was missing `m.priority`, causing unread messages
  to always read back as `normal` regardless of stored value.

## [Unreleased] — idempotency & tracing (WL-026)

> **Per-message idempotency keys and distributed trace IDs.** Callers can supply
> an `idempotency_key` to deduplicate retries (duplicate keys return the existing
> message id). A `trace_id` is auto-minted for every message for end-to-end
> debugging across stores and backends. Both fields propagate through cross-store
> intents (Tier-2) so pulled messages retain their original trace context.

### Added
- **model:** `idempotency_key` and `trace_id` on `Message` and `Intent`;
  `MAX_IDEMPOTENCY_KEY_LEN = 128`, `MAX_TRACE_ID_LEN = 128`,
  `idempotency_key_valid()`, `trace_id_valid()`, and `mint_trace_id()`.
- **store (both backends):** `idempotency_key` and `trace_id` columns on
  `messages` and `outbox` via guarded additive migration; `Store::send` and
  `Store::enqueue_intent` accept both fields; idempotency guard returns existing
  `id` on duplicate key.
- **CLI:** `--idempotency-key` on `weave send` and `weave notify`; trace ID
  auto-minted and surfaced in JSON output.
- **MCP:** `idempotencyKey` optional parameter on `weave_send` and
  `weave_notify`.
- **Tests:** idempotency dedup, trace ID roundtrip, outbox field carry,
  integration JSON shape, and security tests for oversized/hostile keys.

## [Unreleased] — scheduler (WL-016)

> **Daemon-free message scheduling.** One-shot (`--at <unix_ts>`) and recurring
> (`--every <cron>`) message deliveries, evaluated implicitly on every
> `weave hook prompt` and explicitly via `weave tick`. No background process;
> the tick is a cheap read of due schedules + `store.send` per row.
> Mirrored across both storage backends with a guarded additive migration.

### Added
- **model:** `Schedule`, `ScheduleKind` (`OneShot`/`Recurring`), `ScheduleState`
  (`Pending`/`Executed`/`Cancelled`), `MAX_CRON_EXPR_LEN = 64`,
  `cron_valid()`, and `next_occurrence()` — a pure, dependency-free cron
  evaluator supporting presets (`@hourly`, `@daily`, `@weekly`, `@monthly`)
  and simple 5-field cron expressions (`min hour day month dow`).
- **store (both backends):** `schedules` table via guarded idempotent additive
  migration; `schedule_message`, `list_schedules`, `cancel_schedule`
  (soft-cancel), `get_due_schedules`, and `mark_schedule_executed` on the
  `Store` trait. GC prunes old terminal schedule rows.
- **CLI:** `weave schedule`, `weave schedules`, `weave cancel-schedule`,
  `weave tick`. Tick filters self-only by default; `--all` fires every due
  schedule (admin/debug).
- **MCP:** four new tools: `weave_schedule`, `weave_schedules`,
  `weave_cancel_schedule`, `weave_tick`.
- **Hook integration:** `execute_tick` is called best-effort inside
  `handle_hook` `"prompt"` after inbox drain and open-ask nudges.
- **Tests:** CLI roundtrip (schedule → tick → inbox), MCP tool roundtrip,
  cancel idempotency, tick `--all`, and security tests for body/cron/at caps.

## [Unreleased] — workspace split

> Mechanical refactor: the previous single crate is now a Cargo workspace with four
> members (`weave-core`, `weave-inject`, `weave-mcp`, `weave`). No behavior changes.

### Changed
- Split source into workspace crates:
  - `weave-core` — `model`, `config`, `store` (+ `store_libsql`), optional `sign`, and the
    shared `testenv` helper.
  - `weave-inject` — the native mux injector (`inject.rs`) plus the new `Injector` trait
    so the MCP server can accept a mock injector in tests.
  - `weave-mcp` — the MCP stdio JSON-RPC server (`mcp.rs`), now exposing `serve<I: Injector>`.
  - `weave` — the binary crate (`main.rs`, `git.rs`, `setup.rs`, `testenv.rs` re-export),
    plus the moved integration/security/property tests and criterion benchmarks.
- The binary name remains `weave`; integration tests still resolve it via
  `CARGO_BIN_EXE_weave`.
- Feature flags still control the storage backend (`sqlite` default, `libsql`) and the
  optional signer (`sign`), now hosted primarily in `weave-core` and propagated by the
  binary crate.

### Added
- `Injector` trait in `weave-inject`, implemented by the binary crate (`RealInjector`)
  and usable by tests as a mock seam.
- `WorktreeTags` moved into `weave-core::model` so the `Injector` trait can expose
  git-tag capture without adding an `mcp → git` dependency.

## [Unreleased] — presence daemon (v0.2, WL-002 Phase A)

> **Optional background heartbeat daemon + three-tier liveness.**  The daemon
> (`weave daemon start|stop|status|run`) writes periodic heartbeats to a new
> `presence` table every 15 s and evicts stale rows every 60 s.  Display surfaces
> gain a `peer_liveness` resolver: **Live** (fresh heartbeat ≤ 30 s) beats
> **Likely** (last_seen within 900 s TTL) beats **Offline**.  When the daemon is
> stopped the system degrades transparently to the existing TTL heuristic.  No
> new dependency; mirrored across both storage backends with a guarded additive
> migration.

### Added
- **model:** `Liveness` enum (`Live`/`Likely`/`Offline`) with `as_str()`; the
  daemon-tier classifier distinct from the host-aware `store::Liveness`.
- **store (both backends):** `presence` table (`name` PK, `host`, `pid`,
  `heartbeat_ts`) via a guarded idempotent additive migration; `heartbeat`
  (upsert), `presence` (fresh read), `evict_stale_presence(cutoff_secs)`
  (parameterized delete), and `peer_liveness` (default trait method, three-tier
  fallback) on the `Store` trait.
- **CLI:** `weave daemon` subcommand with `start`, `stop`, `status`, and
  `run --me`.  PID file defaults to `$XDG_RUNTIME_DIR/weave/weaved.pid` or
  temp fallback; overridable via `WEAVE_PIDFILE` for test parallel safety.
  argv-only `kill -0` / `kill -TERM` probes (no shell).
- **Tests:** unit tests for heartbeat/query/evict/tier logic in both sqlite and
  libsql backends; black-box integration test `daemon_lifecycle_start_stop_status`
  driving the compiled binary.

## [Unreleased] — presence daemon MCP tools (WL-002 Phase B)

> **MCP tooling for the optional presence daemon.** Three new MCP tools expose the
> daemon lifecycle over JSON-RPC: `weave_daemon_start` (idempotent start, returns pid),
> `weave_daemon_stop` (SIGTERM + cleanup), and `weave_daemon_status` (running or stopped).
> The tools duplicate the small pidfile logic directly (no dependency on the `weave` bin
> crate, respecting the layer DAG). When the daemon is absent, presence degrades
> transparently to the TTL heuristic unchanged.

### Added
- **MCP tools (`weave-mcp`):** `weave_daemon_start`, `weave_daemon_stop`,
  `weave_daemon_status` — each returns JSON-shaped text (`{"started":bool,"pid":u32}` /
  `{"stopped":bool}` / `{"running":bool,"pid"?:u32}`). The tools use argv-only `kill -0` /
  `kill -TERM` and honour the `WEAVE_PIDFILE` env override for test parallel safety.
- **Integration test:** `mcp_daemon_start_stop_status_roundtrip` — start via MCP,
  status confirms running, stop via MCP, status confirms stopped, using a temp-scoped
  `WEAVE_PIDFILE`.
- **Docs:** README daemon subsection; ARCHITECTURE.md optional-daemon section;
  docs/TESTING.md daemon lifecycle test notes.

## [Unreleased] — zellij pane targeting (WL-003)

> **Zellij injection now targets the correct pane instead of the focused one.**
> `detect_target()` captures `ZELLIJ_PANE_ID` (stored in `Peer.socket`, reusing the
> existing auxiliary-id column with no DB migration). `commands_for` threads it through
> as `zellij action write-chars --pane-id <id>` (and `write --pane-id <id>`). When the
> pane id is absent (legacy peers or pre-change registrations), behaviour falls back to
> the focused pane unchanged.

### Changed
- **inject (`weave-inject`):** zellij `commands_for` arm now accepts `--pane-id` when
  `Target.socket` is non-empty.
- **detect (`weave-inject`):** `ZELLIJ_PANE_ID` is read alongside `ZELLIJ_SESSION_NAME`.
- **Docs:** `ARCHITECTURE.md` mux table and paste-safe notes updated.

## [Unreleased] — notify_peer + delivery observability (weave⊇repowire parity, epic 6 / P6)

> **A fire-and-forget notify primitive + a transport-side delivery trace, both pure-DB
> and daemon-free.** `weave notify` / `weave_notify` is a thin no-reply notification over
> the existing send + the P1 honest-verdict seam: it persists a normal stored message,
> fires the SAME caller-side live nudge `weave_send` does, and RETURNS the normalized
> HONEST verdict token (`transport_delivered` / `queued_next_turn` /
> `recipient_not_injectable`). It does **not** fork send and opens **no tracked thread**
> (the difference from `weave_ask`); an unknown peer is honest success (the message waits
> in the store), not an error. Point-to-point only — broadcast notify is deferred (use
> `weave send`). The new **`delivery_log`** table is weave's first *transport-state*
> surface (read receipts are *read-state*): a **metadata-only, SECRET-FREE** per-delivery
> trace of stages (`queued → injected / inject_failed / not_injectable → drained`) written
> **best-effort, caller-side, AFTER the inject** at every send/notify/ask/answer/drain
> point — the store records the OUTCOME it is passed and **never injects** (no
> `store → inject` edge). It stores ONLY `(ref_id, ref_kind, to_peer, stage, outcome, ts)`
> — **never the body, subject, sig, or any token**. Always-on but **bounded** (reads capped
> at `MAX_DELIVERY_ROWS`) and **pruned by the existing `gc()` retention** (no new sweeper).
> **No new dependency**; mirrored across both storage backends with a guarded additive
> migration (a legacy DB upgrades in place); local-mesh only.

### Added
- **model:** `DeliveryRefKind` (`message`/`notify`/`ask`), `DeliveryStage`
  (`queued`/`injected`/`inject_failed`/`not_injectable`/`drained`), `DeliveryOutcome`
  (`ok`/`fail`) enums (`as_str`/`from_str`, the `AskState` enum-as-TEXT precedent);
  `DeliveryTrace` value struct (metadata-only); `MAX_DELIVERY_ROWS = 500`.
- **store (both backends):** `delivery_log` table (6 metadata columns) via a guarded
  idempotent additive migration (legacy DB upgrades in place); `record_delivery`
  (single parameterized INSERT, records the passed-in outcome — no inject) and
  `list_delivery` (oldest-first, bounded by `MAX_DELIVERY_ROWS`) trait methods;
  `gc()` extended to prune `delivery_log` by the same retention cutoff in the same
  transaction. libsql `record_delivery` traps on a read-only handle first
  (owner-only-writes). All SQL parameterized; the only inlined literals are constant
  identifiers + the enum `as_str` constants.
- **mcp:** `weave_notify` (thin over `store.send` + the P1 `ask_delivery_verdict`
  helper; broadcast/oversized → `isError`, unknown peer → honest verdict) and
  `weave_delivery` (read-only trace; unknown ref → empty-trace line, not an error)
  tools; best-effort `record_delivery_best_effort` (logs to stderr, never sinks
  delivery); the pure `verdict_to_stage` fold; trace writes in send/ask/answer.
- **main:** `weave notify` and `weave delivery [--json]` CLI subcommands; the
  caller-side `inject_and_trace` (injects, records queued + the post-inject stage,
  returns the honest verdict); the drain-side `drained` trace at the `prompt`
  mark-read branch (best-effort, after the drain).

## [Unreleased] — rich presence: turn_state + description (weave⊇repowire parity, epic 5 / P5)

> **Daemon-free rich presence, pure-DB.** Three additive `peers` columns — `turn_state`
> (a `TurnState::{Unknown, PendingFirstTurn, Working, AwaitingInput, Idle}` enum stored
> as TEXT, never free text), `description` (a free-form self-set string, ≤200 chars,
> control-stripped), and `description_ts` (its read-time TTL anchor). **turn_state is
> auto-set by the lifecycle hooks** — session→`pending_first_turn`, prompt→`working`,
> stop→`idle`, notification→`awaiting_input` — as a best-effort write that runs *after*
> the drain/registration and can never sink delivery; an explicit `weave status <state>`
> / `weave_set_turn_state` setter exists too (enum-validated; a non-enum value is
> rejected). **description** (`weave describe <text>` / `weave_set_description`) carries a
> **900 s read-time TTL** (`DESCRIPTION_TTL_SECS`, the `ONLINE_TTL_SECS` value but an
> independent constant so it ages out independently of liveness): a stale description
> reads blank, computed at read time by the pure `model::expire_description` — **no
> sweeper**, the stored row untouched. Both setters are **owner-only** (UPDATE bound to
> the caller's own row). Surfaced **compactly and non-noisily** in `peers`/`sessions`/
> `scan` (a `[working]`/`[awaiting-input]`/`[pending]` marker only for a non-idle
> turn_state, a `"…"` suffix only for a live description) and **always** in `weave_whoami`;
> `--json` only ADDS keys. An unset peer's human output is **byte-identical** to pre-P5.
> **No new dependency** (a `#![recursion_limit = "256"]` compile-time attribute was added
> for the larger MCP tool registry — an attribute, not a crate); no `store → inject` edge;
> mirrored across both storage backends with guarded additive migrations (a legacy DB
> upgrades in place reading `unknown`/empty); local-mesh only.

### Added
- **model:** `TurnState` enum (`as_str`/`from_str`, `Unknown` default ⇒ `''`; unknown
  value ⇒ hard `Err`) — the `PeerRole`/`AskState` enum-as-TEXT precedent; `MAX_DESC_LEN =
  200` and `DESCRIPTION_TTL_SECS = 900`; the pure `expire_description(&mut Peer, now)`
  read-time TTL helper (totality via `saturating_sub`, never mutates the stored row);
  three `#[serde(default)]` `Peer` fields `turn_state` / `description` / `description_ts`.
- **store (both backends):** three additive `peers` columns (guarded idempotent
  migration); `set_turn_state` (enum-validated, never stores raw) and `set_description`
  (`sanitize_tag(_, MAX_DESC_LEN)` — oversized truncates, control-stripped; clear stamps
  `description_ts=0`, set stamps `now()`) trait methods, both **owner-only UPDATE-by-name**;
  the read-time `expire_description` applied at the `get_peer`/`list_peers` read seam.
  `register_peer_full` OMITS the three columns from the upsert (the `role` discipline — a
  re-register preserves a self-set turn_state/description). All SQL parameterized; the only
  inlined turn_state literals are the compile-time `TurnState::as_str` constants.
- **main:** lifecycle hooks auto-set `turn_state` best-effort after the drain/registration
  (the previously-reserved `notification` arm now sets `awaiting_input`); `weave describe
  <text>` and `weave status <state>` CLI subcommands (self-only, identity-bound);
  non-noisy `turn_state`/`description` surfacing in `peers`/`scan`/`sessions --watch`
  (human + JSON).
- **mcp:** `weave_set_turn_state` / `weave_set_description` tools (owner-only, the
  `tool_attach` caller-bound precedent; bad turn_state ⇒ Err, oversized description
  truncates rather than erroring); `turn_state`/`description` in `weave_peers`/`weave_scan`
  (compact) and always in `weave_whoami`; additive JSON keys.

## [Unreleased] — circles + orchestrator role (weave⊇repowire parity, epic 4 / P4)

> **Coordination topology, daemon-free and pure-DB.** Two additive `peers` columns —
> `circle` (a visibility-scoping group, default `"default"`) and `role`
> (`PeerRole::{Peer, Orchestrator}`, an enum stored as TEXT, never free text). Legacy
> rows read `circle='default'`/`role='peer'`, so a single-circle deployment is
> **byte-identical** to before. `weave peers`/`sessions`/`scan` default to the caller's
> circle (an orchestrator caller defaults to mesh-wide); `--circle <c>` / `--all-circles`
> (`circle='*'`) scope explicitly. New `weave orchestrator claim [--circle] [--force]` and
> `weave orchestrator status [--circle]`: a single per-circle coordinator, claimed (never
> self-asserted at registration — a re-register PRESERVES an existing orchestrator). A
> non-force claim is **refused** while a LIVE holder exists; `--force` steals it in ONE
> transaction (demote every other orchestrator in the circle → set the caller), a
> **non-destructive** role-bit flip (no confirm gate). "Live" REUSES the existing
> `is_alive` verdict (no new probe, no daemon). New MCP tools `weave_claim_orchestrator` /
> `weave_orchestrator_status`, a `circle` arg on `weave_peers`/`weave_sessions`/
> `weave_scan`, and circle+role in `weave_whoami`. `WEAVE_CIRCLE` env + config `circle`
> key resolve the circle like `WEAVE_SESSION` resolves identity. No new dependency;
> mirrored across both storage backends with guarded additive migrations.

## [Unreleased] — poll-only job board (weave⊇repowire parity, epic 3)

> **Durable, daemon-free work queue.** A persistent `jobs` board on top of the same
> store: workers **poll and claim** jobs and report progress/results back. **Poll-only**
> — there is **no autonomous dispatch or agent-spawn** in this release (a JobRunner that
> *acquires and runs* a job by spawning an agent, plus the cron scheduler and
> dispatch-lease ledger, are deferred to a later runner epic). Claim mints a fresh
> `attempt_id` fencing token; an update with a **stale token on a claimed job is rejected**
> (`stale_attempt`), enforced in the **store** so CLI and MCP both inherit it. Cancel is
> **cooperative** (a worker honors the flag — no daemon needed to request it), never a hard
> delete. **No new dependency**; no `store → inject` edge; dual-backend additive guarded
> migration; runner-only columns excluded; local-mesh only.

### Added
- **store:** new `jobs` table (both backends, guarded idempotent migration — a legacy DB
  upgrades in place, inert plain data in every build). Seven `Store` methods — `create_job`
  (starts `queued`, mints the `job_<…>` id, owner defaults to creator), `get_job`,
  `list_jobs` (filters by state/owner/creator/assignee/circle, `clamp_limit`-bounded,
  `ORDER BY updated_ts DESC`), `claim_job` (mints `attempt_id`, → `running`, rejects
  terminal), `update_job` (**`attempt_id` fencing** + `JobState::can_transition` legality +
  append-only progress event + terminal `completed_ts` stamp), `job_result` (terminal
  payload else `not_ready`), `cancel_job` (cooperative: queued → terminal `cancelled`; else
  flag-only) — mirrored across `store.rs` (sqlite) and `store_libsql.rs` (libSQL, each WRITE
  opening with the `guard_writable()?` write-trap). All SQL parameterized; the only inlined
  literals are the compile-time `JobState::as_str()` constants (BROADCAST-literal discipline,
  with a round-trip drift-guard test). **Runner-only columns excluded** (lease / runner-owner
  / attempts-ledger / cron / schedule / spawn-exec); only the single first-class `attempt_id`
  fencing token is promoted. No `store → inject` edge.
- **model:** `JobState` enum — a **forward-compat 11-state superset** with a frozen terminal
  set (`{Completed, Failed, Cancelled, Expired, Unavailable}`) and the pure monotonic
  `can_transition` (cancel/expire interrupt any non-terminal; no edge out of a terminal,
  idempotent self-noop excepted). `Job` / `JobSpec` / `JobFilter` / `JobPatch` /
  `JobResultView` owned types (`#[serde(default)]` on nullable fields); the opaque id helpers
  `new_job_id` (`job_<seed>_<nonce>`) / `new_attempt_id` (`att_<seed>_<nonce>`, no `rand` /
  date crate) and `job_id_valid` / `attempt_id_valid` + `MAX_JOB_ID_LEN` /
  `MAX_ATTEMPT_ID_LEN` / `MAX_JOB_TEXT` / `MAX_JOB_JSON` caps.
- **mcp:** `weave_job_create` / `weave_job_list` / `weave_job_show` / `weave_job_status` /
  `weave_job_claim` / `weave_job_update` / `weave_job_result` / `weave_job_cancel` tools
  (`show` and `status` are aliases of one status view). Tested failure paths: stale
  `attempt_id` → fenced JSON-RPC error; unknown job → not_found; illegal transition → error;
  oversized title / JSON over the byte cap → cap error; metachar job id → validator error.
  No injector involvement; stdout discipline preserved (only result/error frames).
- **cli:** `weave job create / list / show / status / claim / update / result / cancel`, each
  with `--json`, routing through the **same** seven store methods so fencing / transition /
  caps are enforced once.

## [Unreleased] — ask-many / ask-many-result (weave⊇repowire parity, epic 2)

> **Fan one question to N peers, daemon-free.** Builds directly on the P1 `asks` table:
> `ask_many` opens a parent anchor and creates **one normal P1 `ask` per peer**, fires
> each child's caller-side live nudge, and returns the `parent_id` + per-child
> correlation_ids + honest verdicts immediately (non-blocking). `ask_many_result` is a
> **read-time aggregate** — no background ticker, no stored deadline. **Best-effort**: a
> bad peer is a per-child error, not a whole-call failure (matching repowire). **No new
> dependency**; no `store → inject` edge (per-child nudge fired caller-side); explicit
> peer list only (circles compose later); local-mesh only.

### Added
- **store:** new `ask_groups` parent table + the additive nullable `asks.parent_id`
  column (both backends, guarded idempotent migration — a legacy P1-era DB upgrades in
  place with `parent_id = NULL` for existing asks). New `Store` methods `create_ask_many`
  / `ask_many_result`, mirrored across `store.rs` (sqlite) and `store_libsql.rs` (libSQL,
  with the write-trap as the first statement of `create_ask_many`). Each child is inserted
  via the same factored ask-insert the plain `ask` uses (`parent_id = Some(group)` vs
  `None`) — the P1 lifecycle is shared, not duplicated. Fan-out bounded by
  `MAX_ASK_MANY_TARGETS = 64` (empty / over-cap is a hard whole-call error; the list is
  de-duped). `ask_many_result` rolls up the children at read time with the totality
  `answered + acked + pending + failed == target_count` and classifies
  `complete | partial | pending`.
- **model:** `AskGroup`, `AskManyChildView`, `AskManyResult`, `AskManyState` (+ `as_str`),
  the **pure** classifier `classify_ask_many` (no I/O), the parent-id helpers
  `new_ask_many_id` (`askm_<seed>_<nonce>`, no `rand`/date crate) and `ask_many_id_valid`
  / `MAX_ASK_MANY_ID_LEN`, and an additive `Ask.parent_id` field (`#[serde(default)]`).
- **mcp:** `weave_ask_many` / `weave_ask_many_result` tools. `weave_ask_many` fans the
  question and fires the caller-side nudge **per created child**, attaching each child's
  honest delivery verdict; an unknown/broadcast peer in the list is a per-child error, not
  an `isError`. `weave_ask_many_result` renders the per-child state/answer, the pending
  peer list, the rollup counts, and the `complete | partial | pending` summary
  (read-only). `partial` requires an explicit `age` threshold.
- **cli:** `weave ask-many` (`--to` repeatable, `--body`, `--subject?`, `--from?`) /
  `weave ask-many-result` (`--parent-id`, `--age?`), both with `--json`, reusing the
  caller-side inject-verdict seam per child.

## [Unreleased] — tracked ask/answer/ack (weave⊇repowire parity, epic 1)

> **First step toward repowire capability parity — daemon-free, pure DB.** A
> correlation-tracked request/response on top of the existing messaging + injector,
> distinct from fire-and-forget send/reply. **No new dependency**; no `store → inject`
> edge (the live nudge + delivery verdict are computed caller-side, reusing the existing
> injector return). Both backends gain only the additive `asks` table + the mirrored
> store methods + tests; point-to-point local-mesh only (broadcast / cross-store ask are
> future epics).

### Added
- **store:** additive `asks` side-table (both backends, guarded idempotent migration —
  the `reads`/`revocations` precedent) holding the correlation_id + a monotonic
  `open → answered → acked` lifecycle; the question/answer **text reuses `messages`**
  (threaded via `in_reply_to`), so `asks` carries only correlation + state + pointers.
  New `Store` methods `ask` / `answer` / `ack` / `get_ask` / `list_asks` /
  `ask_for_message`, mirrored across `store.rs` (sqlite) and `store_libsql.rs` (libSQL,
  with the `read_only` write-trap on the three mutating methods). Lifecycle transitions
  are guarded by `model::AskState::can_transition` before any UPDATE — an illegal edge
  (double-ack, answering an acked thread, unknown correlation_id) is a clean error,
  never a panic.
- **model:** `Ask` struct, `AskState` enum (`as_str`/`from_str`/`can_transition` — the
  pure monotonic state machine), `AskRole`, the opaque correlation_id helpers
  `new_ask_id` (no `rand`/date crate — a process-local counter + `now()`) and
  `ask_id_valid` / `MAX_ASK_ID_LEN` (64). `#[serde(default)]` on nullable fields.
- **mcp:** `weave_ask` / `weave_answer` / `weave_ack` / `weave_asks` / `weave_ask_get`
  tools. `ask`/`answer` fire the caller-side nudge and attach an **honest delivery
  verdict** (`transport_delivered` / `queued_next_turn` / `recipient_not_injectable`)
  derived from the existing `inject::capability` + `inject_mode` return — a queued or
  not-injectable ask is **not** an `isError` (degrade-to-store). `weave_answer` accepts
  either a `correlation_id` or an `in_reply_to` message id; broadcast `to` is refused.
- **cli:** `weave ask` / `answer` / `ack` / `asks` / `ask-get` mirroring the tools
  (`--json` on the list/get arms; the caller-side nudge fires via the `try_inject` seam).

## [Unreleased] — observed-revocation audit log + `weave_doctor` verify-summary parity (`sign`)

> **Observability + parity only — `sign`-gated.** R1 revocation is **unchanged**: it
> stays absolute and config-driven (`WEAVE_REVOKED` / `revoked`), and the verifier
> never reads the new table, so the audit log can never weaken or diverge from the
> decision. No new dependency; the default and `libsql`-no-`sign` builds are
> unaffected (the table is inert plain data there). Secret-free throughout —
> fingerprints (`SHA256:<64-hex>`), public identities, source labels, and counts only;
> never a private key, peer pubkey, or token.

### Added
- **store:** additive `revocations` audit table (both backends, guarded idempotent
  migration) — an **observed-revocation log**, write-on-enforce / read-only to the
  decision. A `declared` event is recorded when an operator runs `weave key revoke`;
  an `enforced` event is recorded (best-effort) when the absolute R1 predicate rejects
  a pulled signed intent in `verify_pulled_intent`. An audit-write failure is logged to
  stderr and swallowed — it can never change the rejection.
- **cli:** `weave audit revocations` (human + `--json`, `--limit`) lists the audit log
  most-recent-first, secret-free. `weave doctor` gains `sign_registered_keys_revoked`
  and `sign_revocation_events` (count of registered keys currently revoked + recorded
  revocation events).
- **mcp:** `weave_doctor` gains a `sign`-gated verify summary at **parity** with
  `weave doctor` (strict mode, trusted/revoked counts, registered-key count,
  registered-revoked count, revocation-event count, own fingerprint), closing the
  prior CLI/MCP asymmetry. Counts + the local fingerprint only; appended to the
  JSON-RPC result frame (stdout discipline preserved).

## [Unreleased] — unify the test `WEAVE_*` env guard (`crate::testenv`)

> **Test-only.** No runtime/behavior change, no new dependency (std-only), nothing
> enters the shippable binary (all `#[cfg(test)]`). Contributor-relevant hardening of
> the multithreaded unit-test harness.

### Tests
- **testenv:** new `#[cfg(test)] mod testenv` (`src/testenv.rs`) provides ONE
  canonical, process-wide env serialization guard — `lock_env()` (a poison-tolerant
  `OnceLock<Mutex<()>>`) plus the RAII `EnvVarGuard` (sets/removes a `WEAVE_*` var and
  restores the exact prior state on Drop, even on panic). It replaces `config.rs`'s
  old private `static ENV_GUARD`: all 11 config sites and `inject.rs`'s previously
  **unguarded** `weave_mux_dir_precedes_system_dirs` test now serialize on the one
  lock, eliminating a rare multithreaded `WEAVE_*` data race. A stress test
  (`env_guard_serializes_concurrent_weave_mux_dir`, 8 threads × 200 iters) proves the
  serialization. Integration/security/prop tests are unchanged (separate process,
  scrubbed env).

## [Unreleased] — `weave doctor` federation-health rollup (token/timeout parity)

> **Observability-only.** No new dependency, no schema/migration, no `Store`-trait,
> SQL, or apply-path change. Closes the surfaced-parity gap from Tier-2 v2: the
> per-source token (`WEAVE_PULL_TOKEN_<LABEL>`) and timeout
> (`WEAVE_PULL_TIMEOUT_MS_<LABEL>`) knobs were already **resolved** and **applied**
> for `pull_from`, but `doctor` only **surfaced** them for `peer_db`. The rollup now
> reports both source kinds symmetrically. Secret-free (tier counts + an ms range
> only, never a token); both backends gain only the new aggregation/integration tests.

### Added
- **config:** `Config::federation_health() -> FederationHealth` — a single secret-free
  rollup holding a `FederationKindHealth` per source kind (`peer_db`, `pull_from`):
  source counts (`total`/`local`/`remote`), per-source token tiers, per-source timeout
  tiers, and the effective-ms range (`ms_min`/`ms_max`, `None` over zero remotes). Plus
  the symmetric `Config::pull_from_remote_token_tiers()` accessor that was missing (the
  sibling of `peer_db_remote_token_tiers`). Read-only aggregation over the same
  `resolve_store_sources_with_tiers` the apply path uses — **no** new network probe.
- **cli:** `weave doctor` now reports the `pull_from` delivery side at parity with
  `peer_db` — a `pull sources:` / `pull tokens:` / `pull timeout:` human block and the
  additive `--json` keys `federation_pull_sources`, `federation_pull_local`,
  `federation_pull_remote`, `federation_pull_token_{per_source,shared,none}`,
  `federation_pull_timeout_{per_source,global,default}`, and
  `federation_pull_timeout_ms_{min,max}`. Emitted only when `pull_from` is configured,
  so a local-only config is byte-unchanged; the ms keys only when a remote pull source
  exists.
- **mcp:** `weave_doctor` mirrors the same three `pull_from` human lines via
  `Config::load().federation_health()` (stdout discipline preserved — JSON-RPC frames
  only). Counts/tiers only; never a token byte.

## [Unreleased] — host-aware liveness on `peers` / `doctor` / `sessions --watch`

> Pure observability/consistency upgrade: **no new dependency, no schema/migration,
> no `Store`-trait or SQL change**, and the `is_alive` truth table is **unchanged**.
> Extends #6's host-aware liveness vocabulary (the `<remote>` marker, the
> `alive (local, pid)` / `alive (local, ttl)` / `alive (remote, ttl)` / `stale`
> reasons, and the `N local-alive, M remote-alive, K stale` breakdown) from
> `weave scan` to the three remaining presence surfaces, so all four speak one
> language. Display-only; both backends gain only the pure delegation unit test.

### Added
- **cli:** `weave peers` now marks a remote-host row with a ` <remote>` marker and
  prints its host-aware liveness reason in `[…]` (consistent with `weave scan`);
  `--json` gains two additive keys — `liveness` (the stable token
  `alive_local`/`alive_remote`/`stale`) and `remote` (bool, `host != this_host`).
- **cli:** `weave doctor` gains a `liveness:` line —
  `N local-alive, M remote-alive, K stale` — and three additive `--json` counts,
  `peers_alive_local` / `peers_alive_remote` / `peers_stale` (siblings of the
  existing `peers` / `peers_online` / `peers_tagged`).
- **cli:** the `weave sessions --watch` dashboard shows the per-row liveness reason
  + ` <remote>` marker on each row, and its header now carries the same
  three-count `N local-alive, M remote-alive, K stale` breakdown.
- **mcp:** `weave_peers` and `weave_doctor` mirror the same markers, reason
  strings, and three-count summary in their text results (stdout discipline
  preserved — JSON-RPC frames only; diagnostics to stderr).
- **store:** pure `liveness_from_fields(host, pid, last_seen, this_host, now_ts)`
  field-level seam under `liveness_for` (which now delegates to it, byte-identical),
  letting the pure dashboard render classify a `SessionRow` from its loose fields
  without fabricating a `Peer`. No `Store`-trait/SQL/schema change; lives once in
  `store.rs` and is shared by both backends.

## [Unreleased] — multi-key-per-identity registry (true rotation overlap)

> **No new dependency.** Additive, guarded, idempotent schema migration in **both**
> backends; the legacy single-key `keys` table is retained as a deprecated shadow
> (not dropped) and its rows are copied into the new registry on first open. The #3
> single-key verification behavior is **preserved verbatim** — with exactly one
> registered key the decision table is byte-identical; no row flips REJECT→COMMIT.
> The crypto stays `sign`-gated; the registry itself is plain data in every build.

### Added
- **store:** `identity_keys(identity, pubkey, added_ts, PRIMARY KEY(identity,pubkey))`
  registry holding **multiple** keys per identity, enabling true rotation OVERLAP
  (old + new key both verify during a window) — impossible before at the verification
  layer. New `Store` methods `get_keys` (all keys, oldest-first) and `remove_key`;
  `get_key` becomes a most-recent shim; `list_keys` reads the new table. Mirrored in
  both `store.rs` (sqlite) and `store_libsql.rs` (libSQL). `MAX_KEYS_PER_IDENT` (16)
  caps a hostile registry (a duplicate never counts; exceeding it errors, never
  panics).
- **cli:** `weave key remove <identity> <pubkey-or-fingerprint>` prunes a retired key
  (full hex pubkey, or a `SHA256:<64-hex>` fingerprint resolved against the registered
  set). `weave doctor` reports secret-free per-identity key counts
  (`sign_key_identities`, `sign_registered_keys`, `sign_identities_multi_key`).

### Changed
- **store:** `register_key` now **APPENDS** (`ON CONFLICT(identity,pubkey) DO
  NOTHING`) instead of overwriting; re-registering the same key is a no-op.
  `verify_pulled_intent` now commits a signed intent IFF it verifies against **at
  least one registered NON-REVOKED key** for the sender (a revoked key is skipped —
  R1 preserved; verifies-against-none and present-but-invalid still always rejected).
- **cli:** `weave key add` appends (old + new coexist for rotation overlap) rather
  than overwriting; `weave key rotate` keeps the old key registered during the
  overlap window; `weave key list` shows ALL keys per identity, each with its
  fingerprint and a `[trusted]`/`[REVOKED]` tag.

## [Unreleased] — scan surfaces remote-host sessions (host-aware liveness)

> Pure observability upgrade: **no new dependency, no schema/migration, no
> `Store`-trait or SQL change**, and the `is_alive` truth table is **unchanged**
> (it now delegates to the new classifier but returns byte-identical bools at
> every call site). The host-aware liveness logic already existed inside
> `is_alive`; this surfaces its *reason*. Both backends gain only the mirrored
> classifier unit test (the enum is pure and lives once in `store.rs`).

### Added
- **store:** `pub enum Liveness { AliveLocal, AliveRemote, Stale }` + the pure
  `liveness_for(peer, this_host, now_ts)` classifier formalizing the **A2 —
  fail-open by host** rule (same-host pid-authoritative; remote-host TTL-only,
  *never* a cross-machine pid/network probe; an empty host classifies remote).
  `Liveness::token()` yields the stable tokens `alive_local` / `alive_remote` /
  `stale`. `is_alive` now delegates to it (truth table preserved); the pure
  recency predicate is exposed as `is_online_at(last_seen, now_ts)`.
- **cli:** `weave scan` distinguishes remote-host sessions (a ` <remote>` marker)
  and shows a per-row liveness reason — `alive (local, pid)` / `alive (local,
  ttl)` / `alive (remote, ttl)` / `stale` — plus a trailing `summary: N
  local-alive, M remote-alive, K stale` line. `--json` gains two additive keys:
  `liveness` (the stable token) and `remote` (bool, `host != this_host`).
- **mcp:** `weave_scan` mirrors the same `<remote>` marker, reason strings, and
  summary line in its text result (stdout discipline preserved — JSON-RPC frames
  only; diagnostics to stderr).

## [Unreleased] — session presence dashboard (`weave sessions --watch`)

> Read-only, **dependency-light** (std-only — no TUI/signal/async crate) and
> additive: no `Store`-trait, SQL, or schema change. Reuses the existing scan
> model (`federated_peers` + `is_alive`); both backends are unaffected beyond the
> shared gate.

### Added
- **cli:** `weave sessions --watch` renders a live, **read-only** presence
  dashboard — federated peers grouped by `(repo, branch)`, with a header summary
  (sessions / alive / #repos / #branches) and `name·worktree·mux·host·alive` rows,
  truncating a group past 20 rows to `+N more`. It re-renders until Ctrl-C and
  **writes nothing per tick** (at most one owner-only self-refresh before the loop).
  Flags: `--interval <secs>` (clamped to `[1, 3600]`, default 2), `--iterations N`
  (`0` ⇒ loop forever; `N` ⇒ render N frames then exit, for scripting/tests),
  `--repo`/`--branch` exact-match filters (compose with `--watch`), and
  `--watch --json` (a single JSON snapshot, no clear-screen). The in-place redraw
  uses a plain ANSI clear-home gated on a TTY (`std::io::IsTerminal`) and honoring
  `NO_COLOR` / `WEAVE_NO_CLEAR` with a plain escape-free fallback.
- **config:** `clamp_watch_interval` + `WATCH_INTERVAL_MIN_SECS` (1) /
  `WATCH_INTERVAL_MAX_SECS` (3600) — pure, total clamp for the `--watch` interval.

## [Unreleased] — CI: gate the optional crypto path + the libSQL test suite

### Changed
- **ci:** the GitHub Actions workflow (`.github/workflows/ci.yml`) gains two
  columns — **`sign`** (sqlite + `sign`: `clippy --all-targets` + `cargo test
  --features sign`) and **`libsql + sign`** (`clippy --all-targets` + `build` +
  `cargo test --no-default-features --features "libsql sign"`) — so the optional
  Ed25519 signed-identity path is gated in CI on **both** backends, not just
  locally. The existing **`build (libsql backend)`** job now also runs
  `cargo test --no-default-features --features libsql` (and `clippy --all-targets`),
  closing a gap where the libSQL test suite was never exercised in CI. The four
  required-check names (`rustfmt`, `clippy`, `test`, `build (libsql backend)`) are
  unchanged; `sign` and `libsql + sign` are added as required checks once green.

## [Unreleased] — tighten signed identity (trust-set strict, rotation/revocation, fingerprints)

> All behind the existing `sign` feature; the default and `libsql`-no-sign builds
> gain nothing (no new compiled crate — `sha2` was already transitive via
> `ed25519-dalek`). No schema/`Store`-trait change: trust and revocation are
> receiver-local config.

### Added
- **config:** `WEAVE_TRUST` env var (and `trust = [...]` config) — a comma- or
  whitespace-separated list of **trusted** sender fingerprints (`SHA256:<64-hex>`)
  or full pubkey hex. Configuring a non-empty trust set makes **strict verification
  the default** for the senders in it: a trusted sender's unsigned/unverifiable
  pulled intent is **dropped**, while every other sender keeps the advisory model.
  Entries are validated, control-char-rejected, per-entry-capped
  (`MAX_FP_ENTRY_LEN` = 256), deduped, and total-capped (`MAX_TRUST` = 64).
- **config:** `WEAVE_REVOKED` env var (and `revoked = [...]` config) — a list of
  **revoked** fingerprints. A signature that verifies against a revoked key is
  rejected **unconditionally** (absolute for signed messages — even with
  `WEAVE_STRICT_VERIFY=0` / advisory mode). Same validation/cap discipline.
- **cli (`sign`):** `weave key fingerprint` (`--json`) prints this session's
  `SHA256:<16-hex>` fingerprint; `weave key rotate` archives the old private key
  (`0600` backup), generates a new key, registers it, and prints **both**
  fingerprints + config-based overlap guidance (trust both during the window, keep
  the old pubkey registered, then revoke the old fingerprint); `weave key revoke
  <fp>` validates a `SHA256:<64-hex>`/full-pubkey-hex value and echoes the
  `WEAVE_REVOKED=` / `revoked = [...]` line to add (config-driven; no store table).
- **cli (`sign`):** `weave key show` / `weave key list` (`--json`) and `weave
  doctor` now surface fingerprints (and, in `doctor`, the strict mode + trusted /
  revoked counts) — all secret-free (public keys / fingerprints / paths only).

### Changed
- **config:** `WEAVE_STRICT_VERIFY` (and `strict_verify`) is now **tri-state**:
  unset = the trust-set-aware default; `1`/`true` = force strict everywhere;
  `0`/`false` = advisory everywhere — but never re-admits a revoked key's signed
  message. New `strict_verify_override()` accessor preserving the tri-state.
- **store (both backends):** `pull_from_store` / `commit_pulled` take a
  `&VerifyPolicy` (trust set, revocation list, tri-state override) instead of a bare
  `strict: bool`; `verify_pulled_intent` implements the new trust-set-aware decision
  table. **Verification was only tightened** — the table adds two reject cells
  (`trusted+unsigned`, `revoked+valid-sig`); a present-but-invalid signature is
  still always rejected, and no previously-rejected case became a commit.

### Notes
- **Fingerprints** are `SHA256:` + a display prefix of the SHA-256 digest of the
  **raw 32-byte public key** — secret-free, never derived from the private key.
  Trust/revocation match the **full** digest, so the truncated display form can
  never cause a mis-trust.

## [Unreleased] — session scan / identify / tag (repo · branch · worktree)

### Added
- **cli:** a new **`weave scan`** subcommand — scan, identify, and tag running
  sessions. It first refreshes **your own** peer row's git tags (owner-only-writes),
  then lists every (federated) peer joined with liveness and its
  repo/branch/worktree tags. Flags: `--repo` / `--branch` narrow the set by exact
  tag match, and `--json` emits a machine-readable array of
  `{name, repo, branch, worktree, mux, pane, host, alive, origin, foreign}`.
- **mcp:** a new **`weave_scan`** tool mirroring the CLI — refreshes the caller's
  own row tags (never a foreign row), then returns the federated peer listing with
  liveness and tags as text; optional `repo` / `branch` filters (each bounded so a
  hostile/oversized arg is non-fatal).
- **store / model:** sessions are now **tagged at registration** with their
  **repo** (basename of the git toplevel), **branch**, and a canonical
  **worktree id**, captured best-effort from the session cwd. The tags are surfaced
  by `weave scan`, `weave peers` (CLI `--json` + human, and `weave_peers`),
  `weave sessions` (CLI `--json` + human, and `weave_sessions`, via a local-only
  display join), and `weave doctor` (a `peers_tagged` count). Capture is total: a
  git/fs failure (or a non-git cwd) yields empty tags and never sinks registration.
- **store (both backends):** three additive `peers` columns — `repo`, `branch`,
  `worktree_id` (`TEXT NOT NULL DEFAULT ''`) — added by a guarded, idempotent
  in-place migration mirrored in **both** the sqlite and libSQL backends, so a
  pre-existing DB upgrades with old rows reading empty tags.

## [Unreleased] — remote cross-store pull (Tier-2 v2)

### Added
- **config:** a `WEAVE_PEER_DBS` / `WEAVE_PULL_FROM` (and `peer_dbs` / `pull_from`
  config) entry may now be a **remote `libsql://` / `https://` / `wss://` URL**, not
  just a local file path. A source is modeled as a `StoreSource` — a local path **or**
  a remote URL — classified by scheme (`classify_source`); URLs are never
  canonicalized or compared against the local `db_path`.
- **config:** new `WEAVE_PULL_TOKEN` env var (and `pull_token` config key) — the
  Turso auth token used to open remote sources. It is **secret**: redacted in `Debug`
  / logs (never printed), length-capped (`MAX_TOKEN_LEN` = 8192), and rejected if it
  contains control characters. Prefer the env var over the config file.
- **config:** **per-source pull tokens.** A remote source entry may carry an inline
  `LABEL=<remote-url>` prefix (e.g. `PROD=libsql://prod.turso.io`) that selects a
  distinct token from the env var `WEAVE_PULL_TOKEN_<LABEL>`. The LABEL is uppercased,
  charset `[A-Za-z0-9_]`, ≤ `MAX_LABEL_LEN` (64), and is **not** a secret (it only
  names which env var holds the token), so inlining it is safe — unlike the token,
  which must never be inlined. Per remote source the token resolves with precedence
  **per-source `WEAVE_PULL_TOKEN_<LABEL>` → shared `WEAVE_PULL_TOKEN` / `pull_token` →
  none**; a per-source token goes through the same sanitize gate (cap + control-char
  reject) and, if rejected, **falls through** to the shared token. Fully backward
  compatible: an entry with no label (or whose left-of-`=` is not a valid label, or
  whose right side is not a remote URL) behaves exactly as before. `weave doctor`
  gains token-free aggregate tier counts (per-source / shared / none) and a
  `remote tokens:` line — no token bytes are ever printed.
- **store (libsql):** remote sources are opened **read-only** and weave **never
  writes them** — owner-only-writes now holds **cross-machine**. The remote handle is
  SELECT-only on the foreign store, hard-traps every write method (`guard_writable`
  `bail!`s), runs no schema/migration/hardening, and commits land only in the local
  owned store (local per-source cursor advance). Each remote call is bounded by
  `tokio::time::timeout`; a failed or timed-out remote is skipped (existing
  per-source failure isolation), and the bounded single-intent at-least-once contract
  is preserved.
- **config:** **per-source remote-call timeout.** The remote connect/SELECT bound is
  now resolvable per source via `WEAVE_PULL_TIMEOUT_MS_<LABEL>`, riding the SAME LABEL
  namespace (and `LABEL=` prefix) as the per-source token, with precedence **per-source
  `WEAVE_PULL_TIMEOUT_MS_<LABEL>` → global `WEAVE_PULL_TIMEOUT_MS` → default (5000 ms)**.
  Values are parsed and **clamped to `[50, 600000]` ms**; a `0`/unparsable/out-of-range
  value falls through to the next tier (the bound is never disabled). The resolved value
  is carried to the libSQL backend on a new `StoreSource::Remote.timeout_ms` field and
  bounds both the connect and the read SELECTs. `REMOTE_TIMEOUT_MS_DEFAULT` now lives in
  `config` as the single source of truth (the store fallback imports it). `weave doctor`
  and `weave_doctor` gain a token-free `remote timeout:` line (per-source / global /
  default tier counts + effective ms range) and the JSON keys
  `federation_remote_timeout_{per_source,global,default}` and
  `federation_remote_timeout_ms_{min,max}`. The LABEL namespace + per-source token are
  confirmed to cover **both** `pull_from` and `peer_dbs` remotes (one shared resolver).

### Fixed
- **mcp:** MCP stdio mode now resolves its server identity from `basename(cwd)`
  (via the same `resolve_me()` the CLI uses) when neither the `--session` flag nor
  `cfg.session` is set, so tools no longer error `'from' is required`. Only the
  degenerate "unknown" cwd is left unset.

### Note
- **store (sqlite, default build):** the default backend does **not** support remote
  sources — it skips any remote `peer_dbs` / `pull_from` entry with a loud stderr note
  and processes only local sources. Remote sources require a
  `--no-default-features --features libsql` build.
- **config:** source lists now split **comma-first** (`split_source_list`) so a
  remote URL is kept whole; the platform `:` / `;` path-splitting still applies to
  local (non-URL) fragments, so existing local-path configs are unchanged.

## [Unreleased] — cross-store delivery (Tier-2)

### Added
- **store:** Tier-2 cross-store delivery tables + driver — `outbox` (pending
  directed intents the owner queues for recipients in other stores), `pull_cursor`
  (per-source idempotency high-water mark), and `keys` (registered public keys);
  additive trait methods (`enqueue_intent` / `list_outbox` / `outbox_all` /
  `pull_cursor_get` / `pull_cursor_set` and `register_key` / `get_key` /
  `list_keys`) and the `pull_from_store` / `commit_pulled` free functions, all
  mirrored across both backends. **Owner-only-writes:** a sender only writes its own
  outbox; a receiver opens each source `SQLITE_OPEN_READ_ONLY` and commits intents
  addressed to it into its own inbox. Delivery is idempotent (dedup on the source's
  monotonic `outbox.id`); the only re-delivery window is a crash between commit and
  cursor-advance, bounded to at most one intent.
- **cli:** `weave send --to-store <store> [--to-host <host>]` queues a cross-store
  intent; `weave outbox` inspects pending intents (`--json`); `weave pull` pulls +
  commits from configured `pull_from` sources now (also driven by the hook/`watch`
  drain).
- **mcp:** `weave_send` cross-store routing via `to_store` / `to_host` (queues an
  intent; broadcast refused); new `weave_outbox` tool; the `weave_inbox` drain pulls
  cross-store messages when `pull_from` is configured.
- **config:** `pull_from` / `WEAVE_PULL_FROM` (delivery sources, distinct from
  `peer_dbs`, capped at 16); `inject_pulled` / `WEAVE_INJECT_PULLED` (consent nudge,
  **default ON**); `allow_inject_from` / `WEAVE_ALLOW_INJECT_FROM` (narrow the
  inject-eligible subset); `strict_verify` / `WEAVE_STRICT_VERIFY` (drop
  unsigned/unverifiable intents under signed identity).
- **inject:** a pulled cross-store message from an allow-listed source fires the
  existing content-free, paste-safe nudge into the receiver's **own** pane by
  default (fired caller-side; no `store → inject` edge). Residual risk: any source
  on your pull/allow set can, by default, nudge your live pane — disable with
  `WEAVE_INJECT_PULLED=false` or narrow with `allow_inject_from`.
- **feat(sign):** OPTIONAL Ed25519 signed sender identity behind the `sign` Cargo
  feature (new `sign` module + `ed25519-dalek` / `getrandom`, mirroring the `libsql`
  optional-dep pattern). Adds `weave key gen|show|add|list` (only under
  `--features sign`); signs cross-store intents over canonical `(from, to, body)` and
  verifies on commit so a signed `from` is unforgeable and a tampered/spoofed
  signature is always rejected. Private key at `~/.config/weave/ed25519.key` (0600),
  never logged. **The default build links no crypto** (`ed25519-dalek` is absent from
  the default and libSQL shippable dependency graphs).

## [Unreleased] — presence & live-connect

### Added
- **presence:** real liveness — a peer reads online only when within the presence
  TTL **and** (for a peer on this host with a known PID) its process is still
  running; presence fails open for remote / unprobeable peers. `weave peers` /
  `weave doctor` now report *alive*, not "wrote recently".
- **presence:** heartbeat-on-read — `weave peers` and `weave watch` refresh
  `last_seen` (explicit-identity only) so a session stays visible without traffic.
- **cli:** `weave attach` — adopt a running session into the store without a
  restart (re-capture the current pane and upsert the caller's own peer row).
- **cli:** `weave connect --to <peer>` — report a capability verdict
  (live / registered-but-not-alive / not-injectable); a non-injectable / not-alive
  peer is queued (graceful), not an error.
- **mcp:** `weave_attach` and `weave_connect` tools mirroring the CLI; only a
  non-existent peer is an error (`isError:false` for a queued/degraded verdict).
- **store:** read-only multi-store federation (Tier-1) — `weave peers` /
  `weave sessions` aggregate peers/sessions across extra stores, origin-tagged and
  deduped on `(name, host)`; foreign stores opened `SQLITE_OPEN_READ_ONLY` and
  never written; an unreadable store is skipped, not fatal; default-off keeps
  single-store output byte-identical.
- **config:** `WEAVE_PEER_DBS` env + `peer_dbs` config key (federation store list,
  capped at 16); `this_host()` stable per-machine host label.
- **cli/mcp:** `weave doctor` reports `db_is_default` (a non-default `WEAVE_DB`
  hint) and, when federation is configured, configured / ok / skipped store counts.

### Changed
- **store:** additive `peers.pid` + `peers.host` columns (idempotent migration,
  mirrored across both backends); new additive `register_peer_full` trait method
  (`register_peer` preserved as a default forwarding to it).

### Note
- **store:** cross-store *write* / send (Tier-2) — deferred at the time of this
  pass behind the trust-model gate — has since **shipped** using exactly the
  recommended broker-mediated request-pull, owner-only-writes design. See the
  cross-store delivery (Tier-2) section above.

## [Unreleased] — gap-closing upgrade pass

### Added
- `weave doctor` — diagnostics (backend, db, detected mux, peers, Claude on PATH).
- `weave gc --older-than-secs N` — message retention / disk-bound guard; `Store::gc`.
- `--json` machine-readable output for `inbox`, `peers`, `sessions`, and `doctor`.

### Hardened (security / robustness)
- Untrusted `LIMIT` is clamped (negative no longer means unbounded in SQLite).
- Injected text is length-capped (240 chars) — an oversized body can't flood a pane.
- Mux subprocesses run with a 5s timeout — a wedged tmux/zellij can't hang weave.
- `Config`'s `Debug` redacts the libSQL auth token.

### Fixed
- Injector: `WEAVE_MUX_DIR` now takes precedence over the hardcoded system dirs
  (`/usr/bin`, …) when resolving a trusted mux binary. Fixes a CI-only failure
  where a runner-provided `/usr/bin/tmux` shadowed the fake-mux test harness, so
  the liveness probe ran the real tmux against a nonexistent pane and reported the
  test pane dead. An explicit opt-in dir now wins over an ambient same-named system
  binary; the production liveness probe is unchanged.

### Tests
- 25 → 38 tests: lifecycle hooks (session/prompt/stop, guessed-identity peek, malformed
  payloads), `--json`/`doctor`/`gc`, unknown-backend error, injector cap + clamp + gc unit tests.


All notable changes to **weave** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- `weave setup` / `weave uninstall` real implementations (auto-register the MCP
  server and merge Claude Code lifecycle hooks into `~/.claude/settings.json`
  idempotently).
- End-to-end live-injection validation on the zellij target box.
- Optional `weaved` presence daemon (live online/offline, lifecycle eviction).
- Workspace split (`weave-core`, `weave-inject`, `weave-mcp`, `weave`).

## [0.1.0] — 2026-06-02

First working MVP plus a completion pass: a single static Rust binary that gives
each coding-agent session a name and a persistent mailbox, pushes new messages
into a running session's terminal pane via a native multi-mux injector, and
degrades to hook-driven next-turn delivery where no multiplexer is present.

### Added

- **Core data model** (`model.rs`): `Message` and `Peer` types; UNIX-seconds
  timestamps with a date-crate-free UTC formatter (`now`, `fmt_ts`); the
  broadcast alias set (`all`/`*`/`everyone`/`broadcast`) exposed as both a Rust
  check (`is_broadcast`) and a SQL literal (`BROADCAST_SQL`) derived from one
  source so they cannot drift.
- **Persistent store** (`store.rs`): object-safe `Store` trait and the bundled
  `SqliteStore` (rusqlite, WAL, 30 s busy timeout). Tables `messages`, `reads`,
  `peers`; **per-reader read tracking** so a broadcast is delivered once per
  reader; `inbox` returns remaining-unread alongside messages; `sessions`,
  `total_messages`, `clear_inbox` (non-destructive), `clear_all` (destructive),
  and the peer registry (`register_peer` upsert, `get_peer`, `list_peers`).
  Presence via `is_online` / `ONLINE_TTL_SECS` (900 s). On-disk format is
  libSQL-compatible.
- **Native multi-mux injector** (`inject.rs`): `Mux` for **tmux, zellij, kitty,
  wezterm, screen** (and `None`); `detect_target()` reads the environment;
  `commands_for()` is a pure, fully unit-tested function returning exact argv
  tables per mux; `inject()` checks the binary is on `PATH` and degrades
  gracefully (returns `Ok(false)` when not injectable, errors clearly when the
  pane/mux is gone — never crashing the sender).
- **Paste-safe submission**: per-mux idiom so injection never trips a TUI cancel
  in bracketed-paste mode. tmux closes bracketed paste with the hex `ESC[201~`
  sequence before Enter; wezterm uses `--no-paste`; zellij/kitty/screen append a
  carriage return.
- **No-daemon push model**: the sender injects directly into the recipient's
  registered pane (mux CLIs reach any pane/session from any process), so there is
  no relay or broker process — the DB is the only shared state.
- **MCP stdio server** (`mcp.rs`): newline-delimited JSON-RPC 2.0 with
  `initialize` (protocol negotiation over `2024-11-05` / `2025-03-26` /
  `2025-06-18`), `ping`, `tools/list`, `tools/call`, and empty `resources/list`
  / `prompts/list`. Tools: `weave_send` (injects a live nudge when the recipient
  is an injectable peer), `weave_inbox`, `weave_history`, `weave_sessions`,
  `weave_clear` (`scope:"all"` requires `confirm:true`), `weave_peers`. stdout is
  reserved for protocol frames; logging goes to stderr.
- **CLI** (`main.rs`, clap): `mcp`, `setup`, `uninstall`, `send`, `inbox`,
  `peers`, `sessions`, `register`, `inject`, and `hook <event>`. Identity
  resolves as explicit flag > config/`$WEAVE_SESSION` > basename of cwd.
- **Lifecycle-hook auto-delivery**: `weave hook session` registers the session as
  an injectable peer on `SessionStart`; `weave hook prompt` / `weave hook stop`
  drain unread messages to stdout for `UserPromptSubmit` / `Stop`;
  `weave hook notification` is reserved.
- **Configuration** (`config.rs`): optional `~/.config/weave/config.toml`
  overlaid by `WEAVE_*` environment variables — `session`, `backend`, `db`,
  `nudge_template`, and libSQL connection settings. Honors `XDG_CONFIG_HOME` /
  `XDG_DATA_HOME`.
- **Feature-gated libSQL/Turso backend** scaffolding (`store_libsql.rs` behind
  `--features libsql`) for future cross-machine sync; selecting `backend =
  "libsql"` without the feature fails with a clear message.
- **Tests**: 10 unit tests covering store read-tracking and peer upsert/presence,
  history scoping, and the exact injector command tables for every mux.
- **Documentation**: README, PRD, TASKS, plus this completion pass adding
  ARCHITECTURE, CHANGELOG, CONTRIBUTING, and dual MIT / Apache-2.0 licenses.

### Notes

- The crate builds clean (dev + release) with no default features, is
  clippy-clean, and passes 38 tests.
- `weave setup` / `weave uninstall` are fully implemented (MCP register + hook merge);
  Claude Code wiring is manual (see README) until the setup task lands.

[Unreleased]: https://keepachangelog.com/en/1.1.0/
[0.1.0]: https://keepachangelog.com/en/1.1.0/
