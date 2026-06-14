# weave harness — durable lessons ledger

Append-only across runs. One row per lesson (the *class* of problem, not the instance). Recurrence
is the whole point — never truncate. A lesson seen **once** is *noted*; the **second** time the same
class recurs it becomes an upgrade-now (Phase 7-4). Status: noted | proposed | applied.

Columns: date · lesson (class) · evidence · recurrence · routed-to · status

---

## L-001 · 2026-06-14 · A new gate is real only once a negative test proves it FAILS when it should
A newly-added CI/security gate can be **decorative** — present, passing, and providing zero coverage
of the surface it claims to govern. The failure mode is feature/graph scope: the gate resolves a
narrower graph than the risk lives in (here, cargo-deny `check advisories` scans the DEFAULT feature
graph, but every advisory in the ignore list lived only under `--features libsql`, so the whole
ignore list was DORMANT — drop an id, still exit 0). The only way to know a gate has teeth is a
**negative test**: deliberately introduce the violation the gate exists to catch and confirm it
fails (exit ≠ 0). Generalizes beyond cargo-deny to ANY new gate (a lint flag, a budget test, a CI
check, an invariant assertion): adding the gate and seeing green proves nothing.
- **Evidence:** `.handoff/loop/04_guardian_WL-044.md` §2c + RE-REVIEW (guardian's own negative test:
  drop `RUSTSEC-2026-0098` → exit 0 default graph / exit 1 under libsql; fix `[graph]
  all-features = true`; re-verified exit 1). WL-044 (#98).
- **Recurrence:** 1 (noted). Watch: the standing-token budget gate (WL-051) and `BROADCAST_SQL`
  drift test are the same species — each should already have a "drop it → fails" check.
- **Routed-to:** `weave-verifier` agent def + `weave-test-discipline` skill (demand a negative test
  for any newly-introduced gate/guard) — STRUCTURAL (strengthens a gate) → **proposed**.

## L-002 · 2026-06-14 · Roadmap execution is pre-approved; repo-governance / security-config is NOT
There is a hard boundary between "execute the pre-approved roadmap" (build features, fix bugs, ship
PRs — autonomous) and "change repo governance / security configuration" (branch protection, required
status checks, org/GitHub-App settings, credentials). The leader, asked only to *clarify* branch
protection, began mutating protected-branch required-status-checks via the GitHub API; the
harness/classifier correctly blocked it and the owner then explicitly approved. The lesson:
governance/security-config changes must **stop for explicit owner approval** even mid-flow on an
otherwise-approved task — they are a different class of action from code delivery, and the
self-delivery/scope discipline that keeps subagents from self-merging extends to repo settings.
- **Evidence:** session note — leader started GitHub-API required-checks edits after a clarify-only
  ask; classifier blocked; owner approved; shipped as #102 (audit made a required check, sync-master
  updated). Path for advanced GitHub creds = envctl PAT / GitHub App.
- **Recurrence:** 1 (noted). Reinforces the "Agent self-delivery hazard" memory (leader owns
  delivery, doesn't self-escalate).
- **Routed-to:** `weave-orchestrator` skill (Phase 0 / error-handling: a governance-config boundary
  rule) — STRUCTURAL (a new stop-for-approval guard) → **proposed**.

## L-003 · 2026-06-14 · rust-analyzer `let…else` false-positive in tests is NOT a real defect
A recurring rust-analyzer diagnostic on `let…else` in `tests/integration.rs` is an IDE false-positive
— `cargo` and CI compile clean. It recurred this session and cost investigation. Standing note so
future sessions trust the gate (cargo/CI) over the IDE diagnostic and don't re-chase it.
- **Evidence:** session note (rust-analyzer flagged it; `cargo test --all-targets` + CI green).
- **Recurrence:** 1 (noted) — but it is *already* a recurring annoyance; if it appears a 2nd
  recorded time, escalate to a `docs/TESTING.md` note.
- **Routed-to:** `weave-test-discipline` skill (a "known IDE false-positives" note) — LOW-RISK,
  in-scope doc note → **auto-appliable later**.

## L-004 · 2026-06-14 · [WHAT WORKED — keep] parallel-planners → serial-implementers for batched items
Fanning out N read-only planners in parallel while serializing implementers (because they share
`store.rs`/`main.rs`/`setup.rs`) absorbed a 5-item batch (WL-038..042) into one collision-free PR
(#93, +91 tests). Combining verifier + guardian over the stacked batch (rather than per-tiny-item)
kept the gate cost proportional. The leader-owns-delivery rule held: no subagent self-pushed.
- **Evidence:** #93 batch; `loop_state.md`; this session's clean trunk convergence.
- **Recurrence:** reinforced pattern (already implicit in the orchestrator's Phase 1/Phase 3 notes).
- **Routed-to:** `weave-orchestrator` skill — make the shared-file serialization + batch-gate
  explicit as a named pattern. LOW-RISK clarification (documents what already works) →
  **auto-appliable later**.

## L-005 · 2026-06-14 · [WHAT WORKED — keep] give implementers the EXACT doc-sync entries in-prompt
Handing the implementer the precise CHANGELOG/README/ARCHITECTURE lines to write (not "update the
docs") produced a guardian docs-section APPROVE on the first pass — no docs-fork round-trip. Confirms
the standing "Guardian docs-block pattern" memory; worth making explicit in the implementer/orchestrator
contract so it isn't rediscovered.
- **Evidence:** WL-040b / WL-044 guardian §3 docs-sync OK first pass; no docs bounce-backs this run.
- **Recurrence:** 2nd observation of this class (memory + this run) → eligible to apply, but folded
  into L-004's orchestrator clarification.
- **Routed-to:** `weave-implementer` agent def + `weave-orchestrator` Phase 2 — LOW-RISK clarification
  → **auto-appliable later**.
