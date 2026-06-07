# weave — Tasks / Roadmap

## M0 — MVP (DONE ✅)
- [x] Scaffold Rust binary crate + git
- [x] `model.rs` — Message/Peer types, broadcast set, UTC ts formatting (no date crate)
- [x] `store.rs` — SQLite (rusqlite, bundled): messages/reads/peers, per-reader read tracking, sessions/clear
- [x] `inject.rs` — **native injector**: tmux + zellij, `detect_target()`, pure `commands_for()`, `inject()` with graceful degradation
- [x] `mcp.rs` — MCP stdio JSON-RPC 2.0 server; `weave_*` tools; nudge-inject on send
- [x] `main.rs` — clap CLI: mcp/send/inbox/peers/register/inject/hook
- [x] Unit tests (store read-tracking, peer upsert, injector command construction) — 5/5 green
- [x] Builds clean (dev + release); MCP stdio smoke test passes

## M1 — Make it real on the box
- [ ] `weave setup` — auto-register the MCP server (`claude mcp add`) + write Claude hooks (SessionStart→`weave hook session`, UserPromptSubmit→`weave hook prompt`, Stop→`weave hook stop`), merging with existing hooks (rtk, etc.)
- [ ] Bracketed-paste hardening for tmux: close paste mode with hex `ESC[201~` instead of bare Enter, so injection never triggers a TUI cancel mid-tool-call (repowire's documented bug)
- [x] zellij injection: capture `ZELLIJ_PANE_ID`, pass `--pane-id` to `write-chars`/`write` so injection hits the correct pane instead of the focused one
- [ ] **Validate live injection on the zellij target box** (no mux on the build host)
- [ ] Wizard integration: build `weave` in the RTX-5090 image, run `weave setup`
- [x] Decide retirement of `mcp-broker` / `repowire` once weave is proven

## M2 — Storage backend (DONE ✅)
- [x] Extract a `Store` trait (backend-agnostic; app holds `Box<dyn Store>`)
- [x] `SqliteStore` (rusqlite, bundled) — default `sqlite` feature
- [x] **libSQL/Turso backend** (`store_libsql.rs`, `libsql` feature) — async client driven from
  the sync `Store` trait via an embedded current-thread tokio runtime (`block_on`). Local-file mode
  (`Builder::new_local`) + remote mode (`Builder::new_remote` with auth token). Same schema/SQL/semantics.
- [x] **Mutually-exclusive features** — rusqlite and libsql each bundle SQLite, so they collide at
  link time. `default=["sqlite"]`, `sqlite=["dep:rusqlite"]`, `libsql=["dep:libsql","dep:tokio"]`;
  a `compile_error!` rejects both at once. Build libSQL with `--no-default-features --features libsql`.
- [x] Verified: both backends build, clippy `-D`, run (send/inbox/read-tracking/broadcast/sessions match).

## M3 — Robustness & reach
- [x] Optional `weaved` presence daemon: online/offline, lifecycle eviction (pane-exited/session-closed), so `weave_peers` shows live status (implemented in WL-002)
- [x] More mux adapters: kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`), GNU screen (`screen -X stuff`) (implemented in inject.rs)
- [ ] Workspace split: `weave-core`, `weave-inject`, `weave-mcp`, `weave` (bin)
- [x] Config file (`~/.config/weave/config.toml`): default identity, nudge template, mux preference

## M4 — Cross-machine (maybe; only if needed)
- [ ] libSQL embedded replicas (Turso/sqld) for a shared mailbox across machines — this is the concrete trigger to adopt the `libsql` crate
- [ ] Optional phone/web surface

## Notes
- No daemon in M0–M2: the DB is the broker, mux CLIs do the push. A daemon is only needed for live *presence*, not for messaging or injection.
