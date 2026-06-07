---
description: Resume the weave prompt loop through a detached Kimi CLI worker with a low-token Codex supervisor. Use `/prompt-loop resume kimi-cli codex-min`.
metadata:
  type: slash-command
  owner: weave-harness
---

# /prompt-loop

Low-token supervisor entry point for running the weave loop through Kimi Code.
Use this when the user wants Kimi to do the work while Codex only supervises
small, structured state files.

This slash command is mirrored by the Codex-owned `prompt-loop` skill at
`.agents/skills/prompt-loop/SKILL.md`.

## Primary Form

```bash
/prompt-loop resume kimi-cli codex-min
```

## What It Does

For `resume kimi-cli codex-min`, run:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min
```

This launches `kimi -y -p ...` detached, redirects Kimi's full output to
`_workspace/kimi-cli.log`, writes the worker PID to `_workspace/kimi-cli.pid`,
and initializes `_workspace/agent_status.json`.

## Codex Supervisor Contract

In `codex-min`, do **not** continuously read the Kimi terminal or full log. Poll
only small artifacts:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh status
```

Read full logs only on exception:

- `_workspace/NEEDS-HUMAN` exists.
- `_workspace/agent_status.json` has a stale `last_heartbeat_utc`.
- The PID is gone before `state` is `done` or `blocked`.
- `git status --short` shows edits in the wrong worktree.

When a log is needed, read only the tail first:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh status --tail 40
```

## Worker Contract

The Kimi worker must:

- Resume from committed `_workspace/HANDOFF.md`; that file is authoritative.
- Keep `_workspace/agent_status.json` current before and after each phase.
- Write durable artifacts under `_workspace/` instead of printing large logs.
- Retry transient failures. Only true human walls write `_workspace/NEEDS-HUMAN`.
- Run `_workspace/verify-on-resume.sh` before declaring completion.
- On cycle budget, write a fresh committed `HANDOFF.md` for the next autonomous
  session rather than handing git commands to the user.

## Dry Run

To verify the generated prompt and status file without launching Kimi:

```bash
bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min --dry-run
```
