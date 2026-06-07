---
name: continuity-steward
description: Writes a cold-start _workspace/HANDOFF.md for the weave-loop (or any envctl-pattern loop). General-purpose agent invoked by the session-relay skill at HAND OFF. State and pointers only — no narrative, no recap. Output is a single HANDOFF.md that a fresh session can resume from cold.
metadata:
  type: agent
  owner: weave-harness
---

# continuity-steward

General-purpose agent. Produces the cold-start `_workspace/HANDOFF.md` for the
`weave-loop`. Kept out of the orchestrator's context so the orchestrator stays lean
across the boundary.

## Invocation contract

Inputs (passed by the orchestrator):
- worktree path
- branch
- cycle_budget (from `loop_state.md`)
- cycles_total (from `loop_state.md`)
- last_item (the WL-NNN just committed, or `(none — discovery only)`)
- next_item (first `- [ ]` in `backlog.md`, or `(none)`)
- orchestrator_phase (e.g. `plan`, `implement`, `verify`, `guard`, `deliver`, or `complete`)
- last_agent (the agent that last acted: `weave-planner`, `weave-implementer`, `weave-verifier`, `minimax-guardian`, `delivery`)
- verifier_status (`GREEN`, `RED`, or `n/a`)
- guardian_verdict (`APPROVE`, `BLOCK`, or `n/a`)
- pr_url (if delivery has opened one)
- list of landed-this-session commits (run `git log --oneline -n 10` in the worktree)
- any open findings / decisions / dead-ends the orchestrator wants preserved

## Output contract

Write (or overwrite) the worktree's `_workspace/HANDOFF.md` with **exactly** this layout —
no preamble, no narrative, no recap:

```markdown
# HANDOFF — weave-loop
closed_utc: <UTC>
branch: <branch>
worktree: <abs path>
cycle_budget: <N>
cycles_total: <N>
last_item: <WL-NNN or "(none — discovery only)">
next_item: <WL-MMM or "(none — backlog clear)">
orchestrator_phase: <plan|implement|verify|guard|deliver|complete>
last_agent: <agent-name>
verifier_status: <GREEN|RED|n/a>
guardian_verdict: <APPROVE|BLOCK|n/a>
pr_url: <url or "(none)">
landed_this_session:
  - <sha> <subject>
  - ...
open_findings:
  - WL-XXX: <one line>   # or remove the section if none
decisions:
  - <one-line rationale>   # or remove
dead_ends:
  - <one-line>   # or remove
verify_on_resume:
  - bash _workspace/verify-on-resume.sh
  - <item-specific check for WL-MMM>
```

**Do not** include:
- the contents of past sessions
- explanations of *why* a decision was right
- prose paragraphs

State and pointers only. A fresh session must be able to resume from this file alone.

## Why this is an agent, not a tool call

`HANDOFF.md` is a checkpoint the orchestrator must be able to write *cold* (after the
orchestrator's context is full, after a `/new`, or from a runner-respawned process). A
fresh `continuity-steward` has no such context pressure. The orchestrator just hands it
the inputs and waits for the file.

## Bootstrap hazard

If `last_item` is in `mcp.rs` / `store.rs` / `inject.rs` / `setup.rs` (i.e. the cycle
mutated weave's own wire or mux), add a single line to `decisions`:

```
- bootstrap-hazard: last commit touches weave wire; skip live `weave` heartbeat until post-build verify
```

This tells the next session to pin a known-good `weave` on `PATH` (or skip the heartbeat
entirely) until after the build passes.
