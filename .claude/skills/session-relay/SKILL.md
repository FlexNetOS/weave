---
description: session-relay — the durable handoff for the weave-loop (and any loop in the envctl pattern). Two entry points: HAND OFF writes a committed _workspace/HANDOFF.md and broadcasts a weave `relay:handoff`; RESUME reads the committed checkpoint, runs verify-on-resume, and broadcasts `relay:resumed`. Use at the end of a session (cycle budget hit, or STOP) or to cold-start a fresh session in RESUME mode.
metadata:
  type: session-relay
  owner: weave-harness
---

# session-relay skill

The session-boundary primitive for the `weave-loop`. Splits a long-lived loop into short
sessions: the running session commits a checkpoint, then stops; a fresh session (spawned by
the Ralph runner, by a human, or by `RemoteTrigger`) reads the checkpoint and resumes.

Two entry points, both idempotent.

## HAND OFF (running session → checkpoint)

1. **Stop-checks first.** If `STOP` or `NEEDS-HUMAN` already exists under `_workspace/`,
   skip — the previous run already terminated. Just exit.
2. Spawn the **`continuity-steward` agent** with the worktree path + the current cycle's
   items (see `.claude/agents/continuity-steward.md`). It produces the cold-start
   `HANDOFF.md` body in a single pass — keep the orchestrator's context lean.
3. **Write `_workspace/HANDOFF.md`** (overwrite — the steward body is authoritative).
4. **Commit** it: `weave-loop: handoff (at WL-NNN)`. The committed file is the resume signal.
5. **Best-effort weave heartbeat** — broadcast `to:"all"`:
   `weave send --to all --subject "relay:handoff" --body "worktree=<abs> item=WL-NNN reason=cycle_budget"`.
   If weave itself is in the diff this cycle (bootstrap hazard), **skip the heartbeat and
   log the skip** — the committed file is the truth. Re-verify the heartbeat after the
   build passes.
6. **Best-effort one-shot cron** — `CronCreate {recurring:false}` ~3 minutes out, whose
   prompt is self-describing:
   `"/weave-loop resume from _workspace/HANDOFF.md (worktree=<abs>, model=opus)"`.
   This is a session-only cron in this runtime; the committed file is the survives-restart
   signal. A human or the Ralph runner resumes from it.
7. **Stop** — no `ScheduleWakeup`. The next runner iteration spawns a fresh
   `claude -p` process, which is the `/new` effect.

## RESUME (fresh session → continue the loop)

1. `cd` to the worktree recorded in `HANDOFF.md` (`resume.worktree`). If `HANDOFF.md` is
   missing, fall back to the `weave-loop` skill's DISCOVER entry point.
2. **Read the committed `_workspace/HANDOFF.md`** — it is authoritative, not your inbox,
   not any message log. It names the worktree, the worktree branch, the cycle budget, the
   item to continue at, and the **Verify-on-resume** commands.
3. **Run `bash _workspace/verify-on-resume.sh`** in a fresh shell. If it fails, write
   `_workspace/NEEDS-HUMAN` with the captured output and halt. A failing baseline is a
   human wall; do not paper over it.
4. **Bootstrap hazard check** — if the last landed commit touches `mcp.rs` / `store.rs` /
   `inject.rs` / `setup.rs`, pin a known-good `weave` on `PATH` for the heartbeat or skip it
   (see the hazard note in `harness_hub/upgrade-kits/weave.md`).
5. **Broadcast** `relay:resumed` (safe only after the hazard check):
   `weave send --to all --subject "relay:resumed" --body "worktree=<abs> item=<NEXT>"`.
6. **Reset** `cycles_this_session=0` in `loop_state.md`. Bump `cycles_total` by the carry
   from `HANDOFF.md` (if any). Update `last_update`.
7. **Commit** the reset state: `weave-loop: resume (at WL-NNN)`.
8. **Hand back to the `weave-loop` skill** in CYCLE mode. The first cycle will pick the
   top `- [ ]` in `backlog.md`.

## What `HANDOFF.md` must contain

The `continuity-steward` agent produces exactly this layout (see
`.claude/agents/continuity-steward.md`):

```markdown
# HANDOFF — weave-loop
closed_utc: <UTC>
branch: <branch>
worktree: <abs path>
cycle_budget: 3
cycles_total: <N>
last_item: WL-NNN
next_item: WL-MMM
landed_this_session:
  - <sha> <subject>
  - ...
open_findings:
  - WL-XXX: <one line>   # or remove if none
decisions:
  - <one-line rationale, when relevant>
dead_ends:
  - <one-line, when relevant>
verify_on_resume:
  - bash _workspace/verify-on-resume.sh
  - (item-specific checks for WL-MMM)
```

No narrative — state and pointers only. A fresh session must be able to resume from this
file alone.

## Failure modes (and the right answer)

- **Heartbeat sent but inbox empty on resume** — expected. weave is the observable
  heartbeat, not the resume payload. Use `HANDOFF.md`.
- **Same-machine successor inherits identity** — a self-addressed message lands nowhere
  useful. Don't depend on it.
- **`durable:true` cron is not honored** in this runtime; it's session-only. The
  committed `HANDOFF.md` is the survives-restart signal.
- **HANDOFF.md missing on resume** — fall back to DISCOVER; rebuild `backlog.md` from
  `TASKS.md` M1/M3. Do not panic, do not loop.
