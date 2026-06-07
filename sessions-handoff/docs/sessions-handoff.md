# Session Handoff — weave-loop

**Version:** 2026-06-07  
**Scope:** Document the actual session handoff process used by the weave project.  
**Status:** Living document — update after every cycle.

---

## 1. What this is

The weave project uses an **autonomous, resumable loop** (the `weave-loop` skill) to
drive durable on-disk backlog one cohesive change per cycle. This document describes
how that handoff works so any agent can resume cold.

This replaces the earlier theoretical `hf` (Handoff Ledger) PRD. The `hf` CLI is not
implemented; the working system is the weave-loop described below.

---

## 2. Authoritative resume signal

The committed `_workspace/HANDOFF.md` in the loop worktree is the **only**
authoritative resume signal. A same-machine successor inherits the same identity.
Do not rely on inbox messages for resume state.

```
_worktree/
  HANDOFF.md          ← authoritative: closed_utc, last_item, next_item, findings
  loop_state.md       ← cycles_total, cycles_this_session, cycle_budget, status
  backlog.md          ← all WL items with [-]/[x]/[!] status
  DONE                ← terminal sentinel when no unchecked items remain
  STOP                ← kill switch; touch to halt loop
  verify-on-resume.sh ← baseline gate script
```

---

## 3. Entry points

### DISCOVER
No `backlog.md` yet: seed from `TASKS.md` M1/M3, write `loop_state.md`, commit,
stop.

### RESUME
`HANDOFF.md` present:
1. `cd` to worktree recorded in `HANDOFF.md`.
2. `bash _workspace/verify-on-resume.sh`. If it fails, write `NEEDS-HUMAN` and halt.
3. Bootstrap hazard check: if last commit touches `mcp.rs`/`store.rs`/`inject.rs`/`setup.rs`,
   skip heartbeat or pin known-good `weave` on PATH.
4. Broadcast `relay:resumed` if heartbeat is safe.
5. In `loop_state.md`: set `cycles_this_session=0`, bump `cycles_total`, set status.
6. Commit: `weave-loop: resume (at WL-NNN)`.
7. Continue to CYCLE.

### CYCLE
1. Stop-checks: `STOP` exists? halt. No `- [ ]` left? jump to DONE.
   `cycles_this_session >= cycle_budget`? HAND OFF via session-relay.
2. Pick top `- [ ]` item; record `WL-NNN` in `last_item`.
3. Dry-run destructive ops first (default SAFE; `WEAVE_APPLY=1` opts in).
4. Run weave-orchestrator pipeline (phases 1-3): plan → implement → verify.
5. Guardian review (Phase 4) — MiniMax audits against `weave-invariants`.
6. Delivery (Phase 5-6) on APPROVE: commit, push, PR, auto-merge.
7. Update state: flip to `- [x]`, bump counters, set `last_update`.
8. Self-pace: `ScheduleWakeup` 60-270s if work remains; longer if waiting on external.

### DONE
No unchecked items:
1. Run full verify suite, capture output.
2. Write `_workspace/DONE` with evidence.
3. Commit: `weave-loop: DONE (<N> items, evidence inside)`.
4. Stop. Ralph runner exits 0.

### HAND OFF
At cycle budget: call `session-relay` skill, stop.

---

## 4. Workspace layout

```
_workspace/
  HANDOFF.md                  ← authoritative resume signal
  loop_state.md               ← session state
  backlog.md                  ← all WL items
  DONE                        ← terminal sentinel
  STOP                        ← kill switch
  verify-on-resume.sh         ← baseline gate
  01_planner_plan.md          ← phase 1 output
  02_implementer_changes.md   ← phase 2 output
  03_verifier_report.md       ← phase 3 output
  04_guardian_review.md       ← phase 4 output
  references/
    MANIFEST.md               ← tracked reference repos
    features/                 ← per-repo feature inventories
    gaps/                     ← deduplicated gap index
```

---

## 5. State precedence

When state conflicts:
1. Git HEAD, branch, worktree diff, file contents.
2. Committed `_workspace/HANDOFF.md`.
3. `_workspace/loop_state.md`.
4. `_workspace/backlog.md`.
5. `_workspace/DONE`.
6. Chat transcript — non-authoritative background only.

If lower-precedence disagrees with higher, trust the file.

---

## 6. Navigation for resuming agents

### First command on resume

```bash
cd /home/drdave/Desktop/meta/weave-mcp-daemon-tools
bash _workspace/verify-on-resume.sh
```

### Required reading order
1. `_workspace/HANDOFF.md` — authoritative state
2. `_workspace/backlog.md` — what's done, what's next
3. `_workspace/loop_state.md` — cycle counters, budget
4. `TASKS.md` — original M1/M3 source
5. `docs/ROADMAP-v0.2.md`, `docs/ROADMAP-v0.3.md` — architectural direction

### What to read before coding
1. `weave-invariants` skill — security/correctness rules
2. `weave-test-discipline` skill — test layer guidance
3. `weave-orchestrator` skill — implementation pipeline
4. Relevant `ROADMAP-v0.X.md` section for the WL item

---

## 7. Hard rules

1. No write without running `verify-on-resume.sh` first.
2. No commit without fmt + clippy `-D warnings` + test on BOTH backends.
3. No handoff without updating `HANDOFF.md`, `loop_state.md`, and `backlog.md`.
4. No generated state edited manually unless explicitly allowed.
5. No provider-specific memory as authoritative project state.
6. No stale `HANDOFF.md` — if it disagrees with Git, the file is stale; rewrite it.
7. No swarm self-expansion — one item per cycle.

---

## 8. Current backlog snapshot (2026-06-07)

**Completed:** WL-001..WL-013 (all M1/M3 items)

**Next recommended:** WL-014 (Reminder injection for open asks)

**High-impact gaps:**
- WL-014 — Reminder injection for open asks
- WL-015 — Structured question types
- WL-028 — FTS5 full-text search
- WL-029 — Advisory file leases

See `_workspace/backlog.md` for complete list (WL-001..WL-042).

---

## 9. How to update this document

After every cycle:
1. Update the "Current backlog snapshot" section.
2. Update the "Workspace layout" if new files are added.
3. If the handoff process itself changes, update §3.
4. Commit with: `docs: update sessions-handoff.md`.

This is a **living document**. Stale handoff docs are worse than no docs.
