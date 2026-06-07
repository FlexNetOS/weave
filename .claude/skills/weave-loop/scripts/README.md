# Ralph runner

External self-restart runner for the `weave-loop`. Each iteration spawns a **fresh**
agent process (default: `ollama launch claude --model minimax-m3:cloud -- -p`, a clean
context with Ollama Cloud MiniMax — the `/new` effect), which reads the committed
`_workspace/HANDOFF.md`, runs `verify-on-resume.sh`, does up to `WEAVE_BUDGET` cycles,
writes exactly one sentinel (`DONE` / `NEEDS-HUMAN` / `HANDOFF.md`), and exits.

The runner's job is **only** to respawn. Truth lives on disk.

## Launch

```bash
# SAFE (default): refuses destructive applies; commits non-destructive progress.
bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# UNATTENDED APPLY: opt in deliberately. The runner will pass
# --dangerously-skip-permissions to the spawned agent.
WEAVE_APPLY=1 bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# MiniMax implementation + Kimi review after each iteration.
# Kimi review is fail-soft and writes _workspace/kimi-review-<n>.md when configured.
WEAVE_KIMI_REVIEW=1 WEAVE_APPLY=1 bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# Kill switch, any time:
touch _workspace/STOP
```

## Env knobs

| Var | Default | Meaning |
|-----|---------|---------|
| `WEAVE_WORKTREE` | `/home/drdave/Desktop/meta/weave-harness-loop` | Worktree path |
| `WEAVE_BUDGET`   | `3` | Cycles per session before handoff |
| `WEAVE_MAX_ITERS`| `50` | Hard cap on respawns (backstop) |
| `WEAVE_SLEEP`    | `5` | Seconds between respawns |
| `WEAVE_MODEL`    | `minimax-m3:cloud` | Label used in logs |
| `WEAVE_AGENT_CMD` | `ollama launch claude --model minimax-m3:cloud --` | Agent command used before `-p` |
| `WEAVE_AGENT_MODEL_ARGS` | empty | Extra model args for raw `claude`, e.g. `--model opus` |
| `WEAVE_APPLY`    | `0` | `1` → `--dangerously-skip-permissions` |
| `WEAVE_KIMI_REVIEW` | `0` | `1` → run Kimi review after each iteration |
| `WEAVE_KIMI_CMD` | `kimi` | Kimi executable for review hook |

## Exit codes

- `0` — `DONE` sentinel written; evidence inside.
- `2` — `NEEDS-HUMAN` or `STOP` (human wall / kill switch). Inspect `_workspace/`.
- `3` — `MAX_ITERS` hit without a terminal sentinel. Investigate; the loop is stuck.

## Bootstrap hazard

The runner's spawned agent **must not** depend on the live `weave` binary for the
handoff heartbeat in a cycle that mutates weave's own wire or mux code
(`mcp.rs` / `store.rs` / `inject.rs` / `setup.rs`). The committed
`_workspace/HANDOFF.md` is the authoritative resume signal; the heartbeat is
observability. The agent's prompt (above) carries the rule.
