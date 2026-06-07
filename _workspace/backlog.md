# weave-loop backlog

Seeded from `TASKS.md` M1/M3, reordered to surface the item with an existing planner plan
and the open gaps the user flagged.

## Active / high-priority
- [x] WL-001: Workspace split — carve `weave-core`, `weave-inject`, `weave-mcp`, `weave` bin (TASKS.md M3; ROADMAP-v0.2 Phase 1). Merged via PR #30 (sha 82ea6dd).
- [x] WL-002 Phase A: presence daemon store + CLI — merged via PR #32.
- [x] WL-002 Phase B: MCP daemon tools (`weave_daemon_start`/`stop`/`status`) — PR #33 auto-merging.
- [x] WL-003: zellij pane targeting — capture `ZELLIJ_PANE_ID` at registration, pass `--pane-id` to `write-chars`/`write` so injection hits the correct pane instead of the focused one (TASKS.md M1).
- [x] WL-004: Integration tests for daemon lifecycle — env-configurable heartbeat/evict intervals; idempotency + stale-pidfile coverage.
- [x] WL-005: Harden / execute `ralph-weave.sh` unified loop in anger — fixed broken guardian default, added gh pre-flight, stale-report scrubbing, working-tree sanity check, WEAVE_SKIP_GUARDIAN escape hatch.
- [x] WL-006: `weave setup` — auto-register MCP server + write Claude hooks, merging with existing hooks (TASKS.md M1). Implementation verified in `setup.rs` (register_mcp, merge_hooks, idempotent, uninstall; unit tests present).
- [x] WL-007: Bracketed-paste hardening for tmux — close paste mode with hex `ESC[201~` instead of bare Enter (TASKS.md M1). Implementation verified in `weave-inject/src/inject.rs` (`-H 1b 5b 32 30 31 7e` before Enter).
- [ ] WL-008: Validate live injection on the zellij target box (TASKS.md M1).

## M1 — Make it real on the box
- [ ] WL-009: Wizard integration — build `weave` in RTX-5090 image, run `weave setup` (TASKS.md M1).
- [ ] WL-010: Decide retirement of `mcp-broker` / `repowire` (TASKS.md M1).

## M3 — Robustness & reach
- [ ] WL-011: Optional `weaved` presence daemon — online/offline, lifecycle eviction (TASKS.md M3).
- [ ] WL-012: More mux adapters — kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`), GNU screen (`screen -X stuff`) (TASKS.md M3).
- [ ] WL-013: Config file — `~/.config/weave/config.toml` with default identity, nudge template, mux preference (TASKS.md M3).
