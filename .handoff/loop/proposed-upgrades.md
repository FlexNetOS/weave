# Proposed harness upgrades — weave harness (RECORD-ONLY retro, 2026-06-14)

Per the Phase E lightweight retro, NOTHING here is applied now. Each lesson is classified
**STRUCTURAL → owner approval** or **LOW-RISK in-scope → auto-appliable later** (via the standard
feature-branch → PR → auto-merge flow, with a CLAUDE.md change-history row). Gate-touching changes
may only ever *strengthen* a gate, never weaken one.

---

## P-1 (L-001) — Require a negative test for any newly-introduced gate/guard — STRUCTURAL → PROPOSE

**Why:** WL-044's advisory gate passed green while being decorative (ignore list dormant on the
default graph). A green gate proves nothing; only a negative test (introduce the violation → confirm
exit ≠ 0) proves teeth. This touches the QA/verify gate, so it is structural and may only strengthen.

**Targets & exact edits (for owner approval):**
- `.claude/agents/weave-verifier.md` — add to "Cross-boundary checks" (or a new "Gate-integrity"
  bullet): *"When a change ADDS or modifies a gate/guard (a CI check, a lint flag, a budget/drift
  test, a `compile_error!`, a `confirm` gate), prove it has teeth with a NEGATIVE test: deliberately
  introduce the violation the gate exists to catch and confirm it FAILS (exit ≠ 0 / test red).
  Adding a gate and seeing green proves nothing. Report the negative-test result in
  `03_verifier_report.md`."*
- `.claude/skills/weave-test-discipline/SKILL.md` — add a row/note: *"New gate ⇒ negative test
  (drop/break it → must fail). Applies to CI checks, budget tests (e.g. the standing-token gate),
  drift guards (e.g. `BROADCAST_SQL`), and advisory/deny gates — the gate must be shown to fail on
  the exact violation it governs, on the graph/feature-set where the risk lives."*
- `.claude/agents/weave-guardian.md` — Part 1/Part 2: *"For a gate-bearing change, confirm the
  verifier ran a negative test; if absent, BLOCK until the gate is proven to fail when it should."*

**Note:** This is the highest-value upgrade of the run — it generalizes the single best catch.

## P-2 (L-002) — Governance / security-config boundary: stop for owner approval — STRUCTURAL → PROPOSE

**Why:** Roadmap execution is pre-approved; changing repo governance (branch protection, required
status checks, org/GitHub-App settings, credentials) is a different class and must stop for explicit
owner approval — even mid-flow on an otherwise-approved task. The classifier already enforced this;
codify it so the harness states the boundary rather than relying on the external block.

**Target & exact edit (for owner approval):**
- `.claude/skills/weave-orchestrator/SKILL.md` — add to Phase 0 (or Error handling) a guard:
  *"**Governance boundary (stop-for-approval).** Executing the pre-approved roadmap (features, fixes,
  PRs, auto-merge on green) is autonomous. CHANGING repo governance / security configuration —
  branch protection, required status checks, org or GitHub-App settings, credentials/secrets — is
  NOT pre-approved by a roadmap item. If a task drifts into governance/security-config, STOP and get
  explicit owner approval first. Advanced GitHub credential needs route through the envctl PAT /
  GitHub App, not ad-hoc API edits. (Same scope discipline as leader-owns-delivery.)"*

## P-3 (L-003) — Note the rust-analyzer `let…else` false-positive — LOW-RISK → AUTO-APPLIABLE LATER

**Target & exact edit:**
- `docs/TESTING.md` (or `.claude/skills/weave-test-discipline/SKILL.md`) — a "Known IDE
  false-positives" note: *"rust-analyzer may flag `let…else` in `tests/integration.rs`; this is an
  IDE-only false-positive — `cargo test --all-targets` and CI compile clean. Trust the cargo/CI gate
  over the IDE diagnostic; do not re-investigate."*
- In-scope doc note, weakens nothing → auto-appliable on a future cycle.

## P-4 (L-004 + L-005) — Name the batch pattern + EXACT-doc-sync-in-prompt — LOW-RISK → AUTO-APPLIABLE LATER

**Why:** Both are "what worked" — document them so they aren't rediscovered. Pure clarification of
existing behavior; weakens no gate.

**Targets & exact edits:**
- `.claude/skills/weave-orchestrator/SKILL.md` — Phase 1/Phase 2: *"**Batched items (parallel-plan /
  serial-implement).** For a multi-item batch, fan out planners in PARALLEL (read-only, no
  collision); SERIALIZE implementers when items share files (`store.rs`/`main.rs`/`setup.rs`); run a
  SINGLE combined verifier+guardian gate over the stacked batch. Hand each implementer the EXACT
  CHANGELOG/README/ARCHITECTURE doc-sync lines to write (not 'update the docs') — this earns a
  first-pass guardian docs APPROVE with no docs-fork round-trip."*
- `.claude/agents/weave-implementer.md` — reinforce: *"You will be given the exact doc-sync entries;
  write them verbatim in the same change as the code."*

---

## Apply order when owner approves
1. **P-1** first (highest value — institutionalizes the best catch; strengthens the gate).
2. **P-2** (governance boundary).
3. **P-3 / P-4** can be batched into a single low-risk docs/clarification PR at any time (no approval
   needed; standard branch → PR → auto-merge with a CLAUDE.md change-history row each).

No existing change-history decision is being reversed by any of these (checked CLAUDE.md change
history: the 4-agent team, develop-as-PR-target, and the interim workspace are all preserved).
