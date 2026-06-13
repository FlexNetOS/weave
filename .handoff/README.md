# .handoff — continuity layer (weave)

This repo is a member of the FlexNetOS meta workspace. This directory is its continuity layer
(META-ORG-POLICY.md **P7**; design: handoff ADR-0003 + ADR-0004). It supersedes the deprecated
`_workspace/`, `_workspace_prev/`, `sessions-handoff/`, and stray root handoff files — all
**migrated here via `git mv` (history preserved), archived not deleted** (heal not harm; never
downgrade; never delete).

- `context/capsule.json` — who this repo is and what's next (keep accurate).
- `context/PRD.md` — weave Product Requirements (migrated from the repo root).
- State precedence: **Git > witnessed ledger > task cards**. The fleet ledger lives at
  `meta/handoff/.handoff/ledger.db` — no binary state in this directory, git-committed text only.
- `loop/` — autonomous weave-loop state, migrated from the deprecated `_workspace/`:
  - `loop/loop_state.md`, `loop/backlog.md` — live loop state (the WL-001.. backlog).
  - `loop/TASKS.md` — the M0–M3 roadmap (migrated from the repo root).
  - `loop/_done/` — archived prior loop artifacts: the old `_workspace_prev/` snapshot, the
    superseded `sessions-handoff/` framework (its own manifest admits the `hf` CLI "is NOT
    implemented" — it is now, in `meta/handoff/`), planner plans, and prior session prompts.
- `packets/` — resume packets (`hf handoff`). `packets/latest.md` is the canonical pickup
  (migrated from the root `HANDOFF.md`); dated packets are archived session closes.
- `tasks/` — execution cards (`hf task mint --from-kb`, ADR-0003).
- `decisions/` — ADRs for this repo (none yet; fleet ADRs live in `meta/handoff/docs/`).
- `HARNESS-CHANGELOG.md` — harness change history (migrated from the repo root).

**Stayed at the repo root** (canonical living docs, named as source-of-truth in `CLAUDE.md` —
deliberately NOT continuity state): `ARCHITECTURE.md`, `CHANGELOG.md`, `CLAUDE.md`,
`CONTRIBUTING.md`, `README.md`. See `tasks/TASK-0001-refresh-canonical-docs.task.yaml` for the
tracked follow-up to keep `ARCHITECTURE.md`/`CHANGELOG.md` current with the real feature set.
