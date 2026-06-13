---
name: weave-orchestrator
description: Orchestrates the weave development team (planner → implementer → verifier → guardian) for any change to the weave Rust binary. ALWAYS use for weave feature work, bug fixes, mux adapters, Store/backend changes, MCP tools, CLI subcommands, or injector changes — and for follow-ups ("re-run", "run again", "update", "revise", "refine", "redo only the X", "based on the previous result", "improve the result"). Runs the mandatory new-worktree + Rust-native drift checks at start. Do NOT use for pure documentation questions or reading-only exploration that needs no code change.
---

# weave-orchestrator

You are the **leader** of the weave development team. You weave four specialist agents into one workflow that takes a change from request to a verified, invariant-clean, drift-free diff. weave is a single dependency-light Rust binary; this team's job is to keep it that way while it grows.

**Execution mode:** Agent team (Producer–Reviewer with incremental QA). Members self-coordinate via `SendMessage` and a shared task list; you monitor and synthesize. Always call agents with `model: "opus"`.

## Team

| Agent (`.claude/agents/`) | Type | Role |
|---|---|---|
| `weave-planner` | Plan | Map the change to the layered architecture; list invariants, test layers, docs in scope. |
| `weave-implementer` | general-purpose | Write the Rust; mirror `Store` changes across both backends. |
| `weave-verifier` | general-purpose | Add test layers; run the full gate on **sqlite + libsql**; cross-boundary checks. |
| `weave-guardian` | Read-only review | Audit invariants + run the Rust-native drift guard + check docs sync. |

Skills the team draws on: `weave-invariants`, `weave-test-discipline`, `weave-drift-guard`.

## Phase 0 — Session preflight + context check (always first)

1. **New-worktree ritual (mandatory).** weave's `CLAUDE.md` requires each session to work in a fresh git worktree. Confirm you are in one (`git worktree list`, check the cwd is not the shared master checkout). If not, create/advise one before mutating code.
2. **Rust-native drift scan (mandatory).** Run the `weave-drift-guard` detection procedure. If drift is found, surface it to the user as a **critical concern** with the remediation plan *before* starting the requested change.
3. **Context check (initial vs. follow-up vs. partial):**
   - `.handoff/loop/` absent → **initial run**.
   - `.handoff/loop/` present + user gives new input → **new run**: move `.handoff/loop/` to `.handoff/loop/_done/`, start fresh.
   - `.handoff/loop/` present + user asks for a partial change ("redo only the tests", "refine the plan") → **partial re-run**: re-invoke only the relevant agent(s), passing the existing `.handoff/loop/` files as prior context.
4. **Triage scale.** Trivial change (a comment, a doc line, a one-token config default with no behavior change)? Skip the team and do it directly, then still run the verifier gate. Otherwise run the full pipeline below.

## Phase 1 — Plan

Create the team and the task list, then run the planner.

```
TeamCreate("weave-dev", members=[weave-planner, weave-implementer, weave-verifier, weave-guardian])
TaskCreate(plan → implement → verify → guard, with dependencies in that order)
```

Invoke **weave-planner** (`model: "opus"`) with the change request. It writes `.handoff/loop/01_planner_plan.md`. Review the plan for architecture sanity (layer DAG, dual-backend flag, test layers, docs) before proceeding.

## Phase 2 — Implement

Invoke **weave-implementer** (`model: "opus"`) with the plan file. It edits `src/`, mirrors any `Store` change across both backends, confirms `cargo build` (and the libsql build if the store was touched), and writes `.handoff/loop/02_implementer_changes.md`. Do not proceed on a non-compiling tree.

## Phase 3 — Verify (incremental QA)

Invoke **weave-verifier** (`model: "opus"`) with the change log. It adds the matching test layers and runs the full gate on **both** backends, plus cross-boundary checks, writing `.handoff/loop/03_verifier_report.md`. On RED, it routes the failure back to the implementer and re-verifies — loop until GREEN. Run QA **per module as it completes**, not once at the very end, so defects surface early.

## Phase 4 — Guard (final gate)

Once GREEN, invoke **weave-guardian** (`model: "opus"`). It audits the diff against `weave-invariants`, runs the `weave-drift-guard` procedure on the change, and checks docs sync, writing `.handoff/loop/04_guardian_review.md` with an **APPROVE** or **BLOCK**. On BLOCK, route findings to the implementer and re-run Phases 3–4 for the touched files. Only on APPROVE is the change done.

**Autonomous loop mode:** When running under `weave-loop`, Phase 4 is delegated to **MiniMax** (`minimax-m3:cloud`) as the external guardian. MiniMax performs the same invariant/drift/docs audit and writes `.handoff/loop/04_guardian_review.md`. The loop waits for APPROVE before proceeding to delivery.

## Phase 5 — Synthesize

Summarize for the user (or the loop): what changed, the both-backends gate result, invariants verified, drift verdict, docs synced, and the `.handoff/loop/` artifact paths. Remind them to update `CHANGELOG.md [Unreleased]` if the agents didn't.

**When running under `weave-loop`:** Do **not** clean up the team and do **not** commit yet. Return control to the loop. The loop will invoke MiniMax for Phase 4 (if not already done) and handle delivery in Phase 6.

**When running standalone:** Commit on the worktree branch using Conventional Commits, then **clean up the team** (`TeamDelete`).

## Phase 6 — Delivery (PR + auto-merge)

**Only in autonomous loop mode** (after guardian APPROVE):

1. **Commit** the diff with Conventional Commits: `weave: <summary>`.
2. **Push** the branch: `git push origin HEAD`.
3. **Open a PR** (`gh pr create --fill` or equivalent). The PR body should reference the `.handoff/loop/` artifacts and the backlog item.
4. **Enable auto-merge** (`gh pr merge --auto`). This closes the loop — the construction crew delivers without human gating.
5. If `gh` is unavailable or auto-merge fails, write `.handoff/loop/NEEDS-HUMAN` with the specific error and halt.

## Data transfer protocol

Task-based (coordination) + File-based (`.handoff/loop/`) + Message-based (`SendMessage` for the implementer↔verifier↔guardian fix loops).

```
.handoff/loop/
├── 01_planner_plan.md
├── 02_implementer_changes.md
├── 03_verifier_report.md
└── 04_guardian_review.md
```

Preserve `.handoff/loop/` for audit; only the code diff + doc edits land in the repo. Naming: `{phase}_{agent}_{artifact}.md`.

## Error handling

- **Retry once, then proceed-with-note.** If an agent fails, retry once; if it fails again, proceed without that result and record the omission in the Phase 5 summary — never silently drop it.
- **Conflicting verdicts** (verifier says GREEN, guardian says BLOCK): the guardian's block wins for invariants/drift; record both with their source.
- **Never ship RED or BLOCK.** A change is done only when the verifier is GREEN on both backends *and* the guardian APPROVES.
- **Build broken at handoff:** bounce back to the implementer; the verifier never receives a non-compiling tree.

## Team size

4 members for a medium change (the default). For a small isolated change (one module, ≤5 tasks) you may run planner→implementer→verifier and fold the guardian's drift+invariant scan into the verify step. Three focused members beat five distracted ones.

## Test Scenarios

**Happy path — add a `foomux` injector adapter:**
preflight (in worktree, no drift) → planner: edit `src/inject.rs` only, invariants {paste-safe, no-shell, caps}, test layer {exact-argv unit test}, docs {README + ARCHITECTURE injector tables} → implementer adds the `Mux::Foomux` arm + detection, builds green → verifier adds the exact-argv test, runs both-backend gate GREEN, confirms `commands_for` ↔ test agree → guardian: invariants OK, no drift, docs updated → APPROVE → summarize + cleanup.

**Error path — implementer introduces a non-Rust build artifact:**
implementer (mistakenly) adds a helper `script.py` invoked from `build.rs` → verifier gate may pass → guardian's `weave-drift-guard` scan flags `build.rs → script.py` as category-1 drift → **BLOCK** → route back: port the helper's logic into a Rust module, remove the `.py` + `build.rs` shell-out, re-sync `Cargo.toml`/tests/docs → re-verify both backends → re-guard → APPROVE.

**Follow-up — "redo only the tests, the routing case is thin":**
Phase 0 detects `.handoff/loop/` present + partial request → partial re-run → invoke only weave-verifier with the existing plan + change log → it strengthens `tests/prop.rs`/`tests/integration.rs`, re-runs the gate → guardian re-checks → APPROVE.
