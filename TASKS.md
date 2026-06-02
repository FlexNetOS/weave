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
- [ ] zellij injection: verify `--session <name> action write-chars` targets the right pane (vs focused pane); add `--pane`/focus handling if needed
- [ ] **Validate live injection on the zellij target box** (no mux on the build host)
- [ ] Wizard integration: build `weave` in the RTX-5090 image, run `weave setup`
- [ ] Decide retirement of `mcp-broker` / `repowire` once weave is proven

## M2 — Storage backend
- [ ] Extract a `Store` trait (so the backend is swappable)
- [ ] **DECISION (open): rusqlite vs libSQL/Turso crate.** Current = rusqlite (sync, bundled, fast compile; on-disk file is already libSQL-compatible). The Turso `libsql` crate is **async (tokio)** and pulls a heavy dep tree; it only adds value for **remote DBs / embedded replicas / encryption** — all M4 cross-machine concerns. Recommendation: keep rusqlite for local; adopt libSQL behind the trait **when M4 (cross-machine) lands**. No lock-in: the file is interchangeable.
- [ ] (If/when adopted) feature-gate `libsql` backend; introduce a tokio runtime only in that path

## M3 — Robustness & reach
- [ ] Optional `weaved` presence daemon: online/offline, lifecycle eviction (pane-exited/session-closed), so `weave_peers` shows live status
- [ ] More mux adapters: kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`), GNU screen (`screen -X stuff`)
- [ ] Workspace split: `weave-core`, `weave-inject`, `weave-mcp`, `weave` (bin)
- [ ] Config file (`~/.config/weave/config.toml`): default identity, nudge template, mux preference

## M4 — Cross-machine (maybe; only if needed)
- [ ] libSQL embedded replicas (Turso/sqld) for a shared mailbox across machines — this is the concrete trigger to adopt the `libsql` crate
- [ ] Optional phone/web surface

## Notes
- No daemon in M0–M2: the DB is the broker, mux CLIs do the push. A daemon is only needed for live *presence*, not for messaging or injection.
