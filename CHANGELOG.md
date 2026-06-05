# Changelog

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
