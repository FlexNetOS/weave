# Ralph runner

External self-restart runner for the unified `weave-loop`. Each iteration drives a
closed auto-loop that merges the 3 parts into one:

1. **Local agent** (Claude / Kimi) runs the weave-orchestrator pipeline:
   planner → implementer → verifier (phases 1-3).
2. **MiniMax** (`minimax-m3:cloud`) is the external guardian (phase 4):
   reviews the diff against invariants, drift-guard, and docs; writes
   `.handoff/loop/04_guardian_review.md` with **APPROVE** or **BLOCK**.
3. **Local agent** delivers on APPROVE (phases 5-6):
   commit, push, open PR, enable auto-merge.

The runner's job is **only to respawn**. Truth lives on disk.

## Launch

```bash
# SAFE (default): refuses destructive applies; commits non-destructive progress.
bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# UNATTENDED APPLY: opt in deliberately.
WEAVE_APPLY=1 bash .claude/skills/weave-loop/scripts/ralph-weave.sh
```

## Env knobs

| Var | Default | Meaning |
|-----|---------|---------|
| `WEAVE_WORKTREE` | `/home/drdave/Desktop/meta/weave` | Worktree path |
| `WEAVE_BUDGET`   | `3` | Cycles per session before handoff |
| `WEAVE_MAX_ITERS`| `50` | Hard cap on respawns (backstop) |
| `WEAVE_SLEEP`    | `5` | Seconds between respawns |
| `WEAVE_GUARDIAN_CMD` | *(required)* | Guardian command (e.g. `claude --agent guardian`). Must be set explicitly. |
| `WEAVE_AGENT_CMD` | `claude` | Local agent command (plan+implement+verify+deliver) |
| `WEAVE_APPLY`    | `0` | `1` → `--dangerously-skip-permissions` |

## Exit codes

- `0` — `DONE` sentinel written; evidence inside.
- `2` — `NEEDS-HUMAN` or `STOP` (human wall / kill switch). Inspect `.handoff/loop/`.
- `3` — `MAX_ITERS` hit without a terminal sentinel. Investigate; the loop is stuck.

## Bootstrap hazard

The runner's spawned agent **must not** depend on the live `weave` binary for the
handoff heartbeat in a cycle that mutates weave's own wire or mux code
(`mcp.rs` / `store.rs` / `inject.rs` / `setup.rs`). The committed
`.handoff/packets/latest.md` is the authoritative resume signal; the heartbeat is
observability. The agent's prompt (above) carries the rule.
