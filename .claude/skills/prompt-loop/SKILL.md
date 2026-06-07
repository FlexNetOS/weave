---
name: prompt-loop
description: Low-token supervisor for `/prompt-loop resume kimi-cli codex-min`. Launches the weave loop through a detached Kimi CLI worker in YOLO/APPLY mode while Codex polls only small status artifacts and intervenes only for real human walls.
metadata:
  type: loop-supervisor
  owner: weave-harness
---

# prompt-loop skill

Use this skill when the user invokes:

```bash
/prompt-loop resume kimi-cli codex-min
```

This is the low-token supervision path for the weave loop. Kimi does the worker
execution; Codex stays small by polling status files and reading logs only on
exceptions.

## Primary Dispatch

For `resume kimi-cli codex-min`, run:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min
```

This launches Kimi detached, writes the full worker log to
`_workspace/kimi-cli.log`, writes the worker PID to `_workspace/kimi-cli.pid`,
and initializes `_workspace/agent_status.json`.

## Codex Supervisor Contract

Poll status with:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh status
```

Read only small artifacts by default:

- `_workspace/agent_status.json`
- `_workspace/kimi-cli.pid`
- `git status --short`

Read the full worker log only on exceptions:

- `_workspace/NEEDS-HUMAN` exists.
- `_workspace/agent_status.json` has a stale `last_heartbeat_utc`.
- The PID is gone before `state` is `done` or `blocked`.
- `git status --short` shows edits in the wrong worktree.

When a log is needed, read the tail first:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh status --tail 40
```

## Worker Rules

The Kimi worker must resume from committed `_workspace/HANDOFF.md`; that file is
authoritative. It must keep `_workspace/agent_status.json` current, retry
transient failures, and write `_workspace/NEEDS-HUMAN` only for genuine walls
such as sudo, interactive auth, hardware failure, or branch protection requiring
human review.

At cycle budget, the worker writes and commits a fresh `HANDOFF.md` for the next
autonomous session instead of handing git commands to the user.

## Dry Run

Verify the generated prompt and status file without launching Kimi:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min --dry-run
```
