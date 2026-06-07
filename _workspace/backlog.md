# weave-loop backlog

Seeded from `TASKS.md` M1/M3, reordered to surface the item with an existing planner plan
and the open gaps the user flagged.

## Active / high-priority
- [x] WL-001: Workspace split — carve `weave-core`, `weave-inject`, `weave-mcp`, `weave` bin (TASKS.md M3; ROADMAP-v0.2 Phase 1). Merged via PR #30 (sha 82ea6dd).
- [x] WL-002 Phase A: presence daemon store + CLI — merged via PR #32.
- [ ] WL-002 Phase B: MCP daemon tools (`weave_daemon_start`/`stop`/`status`) — next cycle.
- [ ] WL-003: zellij pane targeting — verify/focus `--pane` so write-chars hits the right pane, not just the focused one (TASKS.md M1).
- [ ] WL-004: Integration tests for daemon lifecycle.
- [ ] WL-005: Harden / execute `ralph-weave.sh` unified loop in anger.

## M1 — Make it real on the box
- [ ] WL-006: `weave setup` — auto-register MCP server + write Claude hooks, merging with existing hooks (TASKS.md M1).
- [ ] WL-007: Bracketed-paste hardening for tmux — close paste mode with hex `ESC[201~` instead of bare Enter (TASKS.md M1).
- [ ] WL-008: Validate live injection on the zellij target box (TASKS.md M1).
- [ ] WL-009: Wizard integration — build `weave` in RTX-5090 image, run `weave setup` (TASKS.md M1).
- [ ] WL-010: Decide retirement of `mcp-broker` / `repowire` (TASKS.md M1).

## M3 — Robustness & reach
- [ ] WL-011: Optional `weaved` presence daemon — online/offline, lifecycle eviction (TASKS.md M3).
- [ ] WL-012: More mux adapters — kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`), GNU screen (`screen -X stuff`) (TASKS.md M3).
- [ ] WL-013: Config file — `~/.config/weave/config.toml` with default identity, nudge template, mux preference (TASKS.md M3).
