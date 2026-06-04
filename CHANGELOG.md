# Changelog

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
