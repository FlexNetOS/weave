# Ralph runner

External self-restart runner for the `weave-loop`. Each iteration coordinates two
agents around the weave project build loop:

1. Kimi Code K2.6 runs preflight planning through the `kimi-legacy` launcher
   that owns the logged-in Kimi Code session, against the backlog, handoff,
   previous review, and git status.
2. Ollama Cloud MiniMax runs the implementation/build pass through
   `ollama launch claude --model minimax-m3:cloud -- -p` in a fresh context.
3. Kimi Code K2.6 reviews the MiniMax pass and writes concrete risks/next
   actions for the following iteration.

The MiniMax pass reads the committed `_workspace/HANDOFF.md`, runs
`verify-on-resume.sh`, does up to `WEAVE_BUDGET` cycles, writes exactly one sentinel
(`DONE` / `NEEDS-HUMAN` / `HANDOFF.md`), and exits.

The runner's job is **only** to respawn. Truth lives on disk.

## Launch

```bash
# SAFE (default): refuses destructive applies; commits non-destructive progress.
bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# UNATTENDED APPLY: opt in deliberately. The runner will pass
# --dangerously-skip-permissions to the spawned agent.
WEAVE_APPLY=1 bash .claude/skills/weave-loop/scripts/ralph-weave.sh

# MiniMax implementation + Kimi Code K2.6 preflight/review.
# Kimi uses `kimi-legacy -r 3c6e42cf-090d-4553-a84b-e63fb9c511c1`
# with model `kimi-code/kimi-for-coding` by default.
WEAVE_APPLY=1 bash .claude/skills/weave-loop/scripts/ralph-weave.sh

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
| `WEAVE_KIMI_PLAN` | `1` | `1` → run Kimi Code preflight before MiniMax |
| `WEAVE_KIMI_REVIEW` | `1` | `1` → run Kimi Code review after MiniMax |
| `WEAVE_KIMI_CMD` | `kimi-legacy` | Kimi launcher that owns the logged-in Kimi Code K2.6 session |
| `WEAVE_KIMI_MODEL` | `kimi-code/kimi-for-coding` | Kimi Code K2.6 model alias |
| `WEAVE_KIMI_SESSION` | `3c6e42cf-090d-4553-a84b-e63fb9c511c1` | Kimi Code session ID |
| `WEAVE_KIMI_SESSION_FLAG` | `-r` | Resume flag for the Kimi launcher |
| `WEAVE_KIMI_EXTRA_ARGS` | `--quiet` | Extra args before `-p`; set empty when using the standalone `kimi` binary |

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
