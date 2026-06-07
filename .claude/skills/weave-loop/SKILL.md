---
description: Autonomous, resumable weave-loop — one backlog item per cycle, commits per cycle, hands off at the cycle budget via the session-relay skill. Replaces a single long session with a chain of short ones; cold resume from the committed _workspace/HANDOFF.md. Use when the user wants to work through the M1/M3 roadmap (WL-001..WL-007) one item at a time, in a fresh git worktree.
metadata:
  type: loop
  owner: weave-harness
---

# weave-loop skill

The in-session body of the Ralph loop for the `weave` crate. Drives the durable on-disk
backlog one cohesive change per cycle, verifies across the boundary, commits, then either
self-paces (`ScheduleWakeup`) or invokes `session-relay` HAND OFF at the cycle budget.

The committed `_workspace/HANDOFF.md` is the **authoritative** resume signal; weave is the
observable cross-identity heartbeat. The external runner is
`.claude/skills/weave-loop/scripts/ralph-weave.sh`.

## Entry points

- **DISCOVER** — no `backlog.md` yet (or empty): seed it from `TASKS.md` M1/M3 and write
  `loop_state.md`. Stop after seeding.
- **RESUME** — `HANDOFF.md` present: run `verify-on-resume`, broadcast `relay:resumed`
  (if weave is healthy), reset `cycles_this_session=0`, jump to the first `- [ ]` in
  `backlog.md`.
- **CYCLE** — the normal body (one item per invocation).
- **HAND OFF** — at the cycle budget, call the `session-relay` skill (HAND OFF mode).

## DISCOVER

1. `cd` to the loop worktree (`worktree` from `loop_state.md`).
2. Read `TASKS.md` M1/M3. Mirror the items into `_workspace/backlog.md` as `WL-NNN` slugs with
   the source reference. The seed backlog already lives there; treat it as the truth and
   only re-derive if it's missing or all items are resolved.
3. If `loop_state.md` is missing or the schema is stale, write a fresh one with
   `session_started: <UTC>` (the skill can't read a clock — you supply it), `cycles_this_session=0`,
   `cycles_total=0`, `status: DISCOVER complete — backlog seeded`.
4. Commit: `weave-loop: discover (backlog seeded)`.
5. **Stop** — do not start a cycle on the discovery turn (per HARNESS-UPGRADE-KIT §3.B.1).

## RESUME (cold start from `HANDOFF.md`)

1. `cd` to the worktree recorded in `HANDOFF.md` (`resume.worktree`). If `HANDOFF.md` is
   missing, fall back to DISCOVER.
2. `bash _workspace/verify-on-resume.sh`. If it fails, write `NEEDS-HUMAN` with the captured
   output and halt (a failing baseline is a human wall, not a cycle we can paper over).
3. **Bootstrap hazard check:** if the last landed commit touches `mcp.rs` / `store.rs` /
   `inject.rs` / `setup.rs` (i.e. wire or mux code), the live `weave` binary is suspect. Pin a
   known-good `weave` on `PATH` for the heartbeat, or **skip the heartbeat** and rely on
   `HANDOFF.md` as the resume signal. Re-verify the heartbeat **after** the build passes
   (per the hazard note in `harness_hub/upgrade-kits/weave.md`).
4. If the heartbeat is safe, broadcast:
   `weave send --to all --subject "relay:resumed" --body "worktree=$WORKTREE item=$NEXT"`.
5. In `loop_state.md` set `cycles_this_session=0`, bump `cycles_total` by the carry from
   `HANDOFF.md` (if any), set `status: RESUMED — at item <NEXT>`, update `last_update`.
6. Commit: `weave-loop: resume (at WL-NNN)`.
7. **Continue** to CYCLE.

## CYCLE (one iteration)

> Read state every cycle. Never hold the plan only in context.

1. **Stop-checks (in this order):**
   - `touch _workspace/STOP` exists → halt (kill switch).
   - No `- [ ]` left in `backlog.md` → jump to **DONE** below.
   - `cycles_this_session >= cycle_budget` (read from `loop_state.md`) → invoke
     `session-relay` HAND OFF, then **stop** (no `ScheduleWakeup`).
2. **Pick the top item** — first `- [ ]` line; record `WL-NNN` in `last_item`. If dependency
   order matters, prepend the dependency list. Items already `[!]` are surfaced and skipped.
3. **Dry-run first** for anything destructive (file moves, hook rewrites, branch protection,
   pubspec-level changes). The default is SAFE; `WEAVE_APPLY=1` opts in to apply.
4. **Run the weave-orchestrator pipeline (phases 1-3).**
   - **Phase 1 — Plan:** invoke `weave-planner`. It writes `_workspace/01_planner_plan.md`.
   - **Phase 2 — Implement:** invoke `weave-implementer`. It edits `src/`, mirrors Store changes across both backends, confirms both compile. Writes `_workspace/02_implementer_changes.md`.
   - **Phase 3 — Verify:** invoke `weave-verifier`. It adds matching test layers and runs the full gate on **both** backends (fmt, clippy `-D warnings`, test). Writes `_workspace/03_verifier_report.md`.
   - Do **not** commit the diff yet. Stop before Phase 4.
   - If verifier is **RED**, route findings back to the implementer and retry. Do not proceed.
5. **Guardian review + approve (Phase 4) — MiniMax is the external guardian.**
   Spawn MiniMax (`minimax-m3:cloud` via the configured guardian command) with the uncommitted diff, the plan, the change log, and the verifier report. MiniMax audits against `weave-invariants`, runs the `weave-drift-guard` scan, checks docs sync, and writes `_workspace/04_guardian_review.md` with **APPROVE** or **BLOCK**.
   - If **BLOCK**, preserve the specific findings and route back to the implementer on the next iteration. Do not mark the backlog item as done.
6. **Delivery (Phase 5-6) — on APPROVE only.**
   - Commit with Conventional Commits: `weave: WL-NNN <one-line summary>`. Include updated `backlog.md` + `loop_state.md`.
   - Push the branch: `git push origin HEAD`.
   - Open a PR: `gh pr create --fill`.
   - Enable auto-merge: `gh pr merge --auto`.
7. **Update state** — flip the line to `- [x]` (or `- [!] blocked: <reason>`). Bump
   `cycles_this_session` and `cycles_total` by 1. Set `last_item=WL-NNN`, `last_update=<UTC>`.
8. **Self-pace.** Pick the next delay from what you're actually waiting on:
   - No external wait, no work remaining that needs a human → `ScheduleWakeup` 60–270s
     (cache-warm window) to re-enter CYCLE.
   - Waiting on a slow external step (CI, a remote sync, a human prereq) → `ScheduleWakeup`
     1200s+ with a one-line reason naming the wait.
   - At cycle budget → do **not** `ScheduleWakeup`; call `session-relay` HAND OFF and stop.

## DONE (terminal sentinel — write evidence, then halt)

1. Run the full verify suite once more from a clean shell, capture the output.
2. Write `_workspace/DONE` with the evidence:
   ```
   closed: <UTC>
   cycles_total: <N>
   items_closed: WL-001, WL-002, ...
   evidence:
     - cargo fmt --all -- --check   -> exit 0
     - cargo clippy -D warnings     -> exit 0
     - cargo test                   -> <N> passed, 0 failed
     - (feature smokes per WL-NNN)
   ```
3. Commit: `weave-loop: DONE (<N> items, evidence inside)`.
4. **Stop.** Do not `ScheduleWakeup`. The Ralph runner will exit 0 on `DONE`.

## NEEDS-HUMAN (terminal sentinel — human wall, not a spin)

When the cycle hits sudo / interactive auth / branch-protection-requiring-human-review /
hardware, do **not** force it. Write:

```
reason: <one sentence — what's blocking, what was tried>
artifact: <path to the captured log / diff / error>
last_item: WL-NNN
```

Commit the sentinel. Stop. The Ralph runner exits 2 and surfaces it to the human.

## Backlog discipline

- `- [x]` only after VERIFY passes in a fresh shell. `cargo build` is not verification;
  `cargo test` is. Mark `[x]` only with passing evidence.
- `- [!] blocked: <reason>` when an external prereq is missing. Carry it forward, do not
  silently drop it.
- A new gap discovered mid-cycle becomes a new `- [ ] WL-NNN` with a one-line description;
  never expand the current item past one cohesive change.
- **The committed `HANDOFF.md` is authoritative.** A same-machine successor inherits the
  same identity, and a self-addressed weave message does **not** land in your own inbox.
  Don't rely on your inbox for the resume signal — the file is.
