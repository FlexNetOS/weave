# weave-loop backlog

Legend: `- [ ]` todo · `- [x]` done+verified · `- [!] blocked: <reason>`

Sourced from `TASKS.md` M1/M3 (M0 + M2 already done). One item per cohesive change.
Each cycle commits one item; verify-on-resume + per-cycle verification = `cargo fmt` + `cargo clippy` + `cargo test`.

## M1 — Make it real on the box
- [ ] **WL-001** `weave setup` — auto-register the MCP server (`claude mcp add`) + write Claude hooks
  (SessionStart→`weave hook session`, UserPromptSubmit→`weave hook prompt`, Stop→`weave hook stop`),
  merging with existing hooks. (TASKS.md M1)
- [ ] **WL-002** Bracketed-paste hardening for tmux: close paste mode with hex `ESC[201~` instead of
  bare Enter, so injection never triggers a TUI cancel mid-tool-call. (TASKS.md M1)
- [ ] **WL-003** zellij injection: verify `--session <name> action write-chars` targets the right
  pane (vs focused pane); add `--pane`/focus handling if needed. (TASKS.md M1)

## M3 — Robustness & reach
- [ ] **WL-004** Optional `weaved` presence daemon: online/offline, lifecycle eviction
  (pane-exited/session-closed), so `weave_peers` shows live status. (TASKS.md M3)
- [ ] **WL-005** More mux adapters: kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`),
  GNU screen (`screen -X stuff`). (TASKS.md M3)
- [ ] **WL-006** Workspace split: `weave-core`, `weave-inject`, `weave-mcp`, `weave` (bin).
  (TASKS.md M3)
- [ ] **WL-007** Config file (`~/.config/weave/config.toml`): default identity, nudge template,
  mux preference. (TASKS.md M3)

## Bootstrap hazard (cycle that mutates weave itself)

If a cycle changes weave's wire/inbox behavior (e.g. `mcp.rs`, `store.rs`, `inject.rs`),
**do not depend on the live `weave` binary for the handoff heartbeat that cycle.** The
committed `_workspace/HANDOFF.md` is the authoritative resume signal — weave is only the
observable heartbeat. After the build passes, re-verify the heartbeat works *before* handing off.
