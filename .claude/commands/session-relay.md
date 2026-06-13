---
description: Session boundary primitive for the weave-loop. Two modes: HAND OFF writes a committed .handoff/packets/latest.md and stops; RESUME reads the checkpoint, runs verify-on-resume, and continues the loop. Use at cycle budget, STOP, or cold-start.
metadata:
  type: slash-command
  owner: weave-harness
---

# /session-relay

The **session-boundary primitive** for the `weave-loop`. Splits a long-lived loop
into short sessions: the running session commits a checkpoint, then stops; a fresh
session reads the checkpoint and resumes.

Two modes, both idempotent.

## /session-relay handoff

Called by the `weave-loop` skill when `cycles_this_session >= cycle_budget`.

1. **Stop-checks.** If `.handoff/loop/STOP` or `.handoff/loop/NEEDS-HUMAN` exists,
   skip — already terminated.
2. **Write `.handoff/packets/latest.md`** with the orchestrator state
   (`orchestrator_phase`, `verifier_status`, `guardian_verdict`, `pr_url`,
   `landed_this_session`, `open_findings`, `decisions`).
3. **Commit** it: `weave-loop: handoff (at WL-NNN)`.
4. **Best-effort heartbeat** — broadcast `relay:handoff` via `weave send`.
   Skip if bootstrap hazard (last commit touched mcp.rs/store.rs/inject.rs/setup.rs).
5. **Best-effort one-shot cron** — `CronCreate {recurring:false}` ~3 min out
   with prompt: `"/weave-loop resume from .handoff/packets/latest.md"`.
6. **Stop.** No `ScheduleWakeup`. The Ralph runner spawns the next session.

## /session-relay resume

Called by `/weave-loop resume` or directly on cold start.

1. `cd` to the worktree in `.handoff/packets/latest.md`. If missing, fall back to `weave-loop`
   DISCOVER.
2. **Run** `bash .handoff/loop/verify-on-resume.sh`. If it fails, write
   `.handoff/loop/NEEDS-HUMAN` with output and halt.
3. **Bootstrap hazard check** — if last commit touches mcp.rs/store.rs/inject.rs/setup.rs,
   pin a known-good `weave` or skip heartbeat.
4. **Broadcast** `relay:resumed` via `weave send` (if safe).
5. **Reset** `cycles_this_session=0` in `loop_state.md`. Bump `cycles_total`.
6. **Commit:** `weave-loop: resume (at WL-NNN)`.
7. **Hand back to `weave-loop`** in CYCLE mode.

## Arguments

- `handoff` — running session → checkpoint → stop (default at cycle budget).
- `resume` — fresh session → verify → continue (default on cold start).
- `from .handoff/packets/latest.md` — canonical path (default if omitted).

## One-liner tests

```bash
/session-relay handoff              # at cycle budget
/session-relay resume               # cold start
/weave-loop resume from .handoff/packets/latest.md   # equivalent
```
