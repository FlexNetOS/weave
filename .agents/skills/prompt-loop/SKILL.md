---
name: prompt-loop
description: Codex-side supervisor for `/prompt-loop resume kimi-cli codex-min`. Use when the user wants the weave prompt loop run through a detached Kimi CLI worker while Codex polls only small status artifacts and escalates only on real human walls.
metadata:
  type: prompt-loop
  owner: weave-harness
---

# prompt-loop skill

Use this skill for the low-token weave supervisor path.

For `resume kimi-cli codex-min`, run:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min
```

The worker writes `_workspace/kimi-cli.log`, `_workspace/kimi-cli.pid`, and
`_workspace/agent_status.json`. Poll status rather than tailing the full log
unless one of these happens:

- `_workspace/NEEDS-HUMAN` exists.
- `agent_status.json` has a stale `last_heartbeat_utc`.
- The PID is gone before the worker reports `done` or `blocked`.
- `git status --short` shows edits in the wrong worktree.

When a log is needed, inspect the tail first:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh status --tail 40
```

The worker must resume from committed `_workspace/HANDOFF.md`, retry transient
failures, and write `_workspace/NEEDS-HUMAN` only for genuine walls such as
sudo, interactive auth, hardware failure, or branch protection requiring human
review.

Dry-run the prompt and status path with:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min --dry-run
```
