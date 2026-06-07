---
description: Resume the weave-loop from _workspace/HANDOFF.md in a fresh session. Use when the user says "resume the weave loop", "pick up the loop", "continue in a new session", "/weave-loop resume from _workspace/HANDOFF.md", or the prior session hit the cycle budget and committed a handoff.
metadata:
  type: slash-command
  owner: weave-harness
---

# /weave-loop resume

Resume the **weave-loop** in a fresh session, cold-starting from the committed
`_workspace/HANDOFF.md`. This is the slash-command half of the envctl-pattern
"short-session chain": the prior session hit its cycle budget, the
`session-relay` skill wrote `HANDOFF.md`, and now a new process picks it up.

The committed file is the **authoritative** resume signal — not the weave inbox.
(See the `session-relay` skill, "Failure modes".)

## Arguments

- `from _workspace/HANDOFF.md` — the canonical phrasing. Tells the skill the
  checkpoint is the source of truth, not a fresh DISCOVER.
- (optional) `budget=N` — override the per-session cycle budget (default from
  `loop_state.md`).

## Steps

1. **Locate the worktree.** Read `HANDOFF.md` and `cd` to `resume.worktree`. If
   `HANDOFF.md` is missing, fall back to the `weave-loop` skill's **DISCOVER**
   entry point (it will rebuild `backlog.md` from `TASKS.md` M1/M3 and stop).
2. **Invoke `session-relay` skill, RESUME mode.** It runs `verify-on-resume.sh`,
   does the bootstrap-hazard check, broadcasts `relay:resumed` (if safe),
   resets `cycles_this_session=0`, commits, and hands back to the `weave-loop`
   skill in CYCLE mode.
3. The first CYCLE picks the top `- [ ]` from `backlog.md` and does the work.

## What this command does *not* do

- It does not start a new `claude` process. The Ralph runner
  (`.claude/skills/weave-loop/scripts/ralph-weave.sh`) is what spawns fresh
  processes. This command is the entry point a *human* (or a runner prompt) uses
  to tell a session "you are a resume".
- It does not depend on the weave inbox. Don't `weave_inbox` for the resume
  signal — read the file.

## One-liner test

In a fresh shell, with a `HANDOFF.md` already committed in the worktree:

```bash
/weave-loop resume from _workspace/HANDOFF.md
```

The orchestrator should land in CYCLE mode at `next_item` within a few tool
calls and commit a fresh `weave-loop: resume (at WL-NNN)` along the way.
