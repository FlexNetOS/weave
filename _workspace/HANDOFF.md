# HANDOFF — weave-loop

closed_utc: 2026-06-08T02:00:00Z
branch: feat/zellij-pane-targeting
worktree: /home/drdave/Desktop/meta/weave-mcp-daemon-tools
cycle_budget: 3
cycles_total: 24
last_item: WL-023
next_item: WL-024
orchestrator_phase: complete
last_agent: n/a
verifier_status: GREEN
guardian_verdict: APPROVE
pr_url: (none)
landed_this_session:
  - 84ba88c resume: clippy fixes for WL-020/021 unused imports & vec_init_then_push
  - 94c2102 WL-023: iTerm2 injector backend
  - 29b3175 fix(store): move sqlite-only imports behind cfg flag
open_findings:
decisions:
  - WL-023 completed in cycle 1/3.
  - iTerm2 uses osascript (always on macOS) rather than a dedicated binary.
  - No liveness probe for iTerm2 (fail-open); always treated as alive.
  - Session ID from TERM_SESSION_ID env var (e.g. w0t0p0:ABC123).
  - CLI and MCP automatically support ITerm2 via generic Mux::parse/as_str.
dead_ends:
verify_on_resume:
  - bash _workspace/verify-on-resume.sh
  - cargo test --all-targets
  - cargo test --all-targets --no-default-features --features libsql
