# ADR-0001 — Adopt the Continuity Ledger Kernel (`.handoff`) as weave's only continuity path

- **Status:** accepted — 2026-06-13
- **Plane:** agent-mesh
- **Owner:** drdave
- **Scope:** `weave/.handoff` (full Tier-B design), the weave-loop harness path references, and
  one fleet-registry entry in the kernel repo (`meta/handoff/.handoff/fleet/weave/capsule.json`).
- **Supersedes/relates:** handoff ADR-0003 (kb↔handoff seam), ADR-0004 (fleet rollout / ledger
  residency / policy P7); deprecates weave's `_workspace/`, `_workspace_prev/`, `sessions-handoff/`,
  and the stray root handoff files for *new* state (history preserved, never bulk-deleted).

## Context

The owner directive (2026-06-13): weave must **adopt the full design of meta/handoff**, with the JSON
and ADR files kept **in sync** with the canonical kernel. `ARCHITECTURE.md` and `CHANGELOG.md` stay at
the repo root (canonical living docs, source-of-truth per CLAUDE.md) — not continuity state.

Before this change weave carried continuity state under rival, drifted conventions: `_workspace/`
(loop state), `_workspace_prev/` (a superseded snapshot), `sessions-handoff/` (an *older* framework
whose own manifest admits "the `hf` CLI … is NOT implemented" — it is now, in `meta/handoff`), plus
root `HANDOFF.md`/`PRD.md`/`TASKS.md`/`HARNESS-CHANGELOG.md`. ADR-0004 measured `.handoff/` in 1 of 58
repos and names weave explicitly in its migration list (`_workspace/` → `.handoff/`, keep history).

## Decision

1. **`.handoff/` is weave's only continuity path.** All deprecated handoff state migrated into it via
   history-preserving `git mv`, archived under `.handoff/loop/_done/` — never deleted (upgrade-only;
   heal not harm; never downgrade).
2. **Full Tier-B design** (ADR-0004 §2, weave runs autonomous loops):
   - `context/capsule.json` — REQUIRED `handoff.context_capsule.v1` (project_name, role, plane,
     northstar, next_command); passes the P7 capsule-schema gate.
   - `tasks/*.task.json` — canonical `handoff.task.v1` cards (minted, schema-conformant).
   - `packets/latest.md` — resume packet (the former root `HANDOFF.md`).
   - `loop/{loop_state,backlog,TASKS}.md` + `loop/_done/` — autonomous weave-loop state.
   - `hooks/hooks.toml` (`handoff.hooks.v1`) + `policies/rules.toml` (`handoff.policy.rules.v1`) +
     `policy.toml` (`handoff.policy.v1`) — the loop-automation contract, adopted from the kernel and
     specialized to weave's branch model (develop→master) and dual-backend gate.
   - `README.md` — one-screen contract.
3. **No ledger.db, no binary state in this directory** (ADR-0004 §3): the one witnessed ledger lives
   at `meta/handoff/.handoff/ledger.db`. weave's `.handoff` is git-committed text only.
4. **Fleet registration:** weave is added to the kernel's fleet registry
   (`meta/handoff/.handoff/fleet/weave/capsule.json`), kept **byte-for-byte in sync** on the required
   capsule fields with `weave/.handoff/context/capsule.json`. This is the "JSON in sync" requirement.
5. **Harness rewire:** every weave-loop / session-relay / orchestrator path reference repointed from
   `_workspace/` to `.handoff/` so the loop never recreates the deprecated dir.

## Consequences

- `hf resume` / `hf fleet status` now see weave like any other fleet member; `meta git update` carries
  weave's continuity state as plain git (no daemon, precedence Git > ledger > cards).
- The protected-files policy guards weave's own guardrails (`.github/`, `.handoff/{policy,policies,
  hooks,decisions}`, `CLAUDE.md`, `Cargo.toml`, lockfiles) against agent self-modification.
- `ARCHITECTURE.md`/`CHANGELOG.md` staleness (owner-flagged) is tracked as `tasks/TASK-0001.task.json`
  rather than fixed by moving them — moving canonical docs into `.handoff` would be a downgrade.

## Research / Cross-References

- **Codebase (verified 2026-06-13):** kernel layout `meta/handoff/.handoff/` (active.md, context,
  decisions, fleet/<repo>/capsule.json, hooks, policies, policy.toml, skills, tasks/*.task.json,
  ledger.db); canonical schemas `meta/handoff/schemas/{task,packet,session}.schema.json`
  (`handoff.task.v1`, `handoff.packet.v2`, `handoff.session_event.v1`); `hf init`/`hf seed` in
  `meta/handoff/hf/src/main.rs` (seed writes kernel-specific HFTASK cards → not used for weave).
- **Exemplars:** `envctl/.handoff` (loop-running Tier-B member: README + capsule + decisions/ADR +
  loop/ + packets + tasks; its README documents the same `_workspace/` → `.handoff` `git mv`); `agent/`
  (minimal member); fleet capsules `fleet/{teri,ECC}/capsule.json` (format reference).
- **Governing ADRs:** handoff ADR-0004 (fleet rollout, tiered contents, ledger residency, migration
  list naming weave, policy P7), ADR-0003 (kb↔handoff seam, minted-cards-only).
- **Conformance:** `meta/.github/workflows/p7-conformance.yml` — capsule REQUIRED fields
  {project_name, role, plane, northstar, next_command} and no ad-hoc handoff markdown outside
  `.handoff/`; weave's capsule and migrated layout both pass.
- **Verification:** dual-backend gate green (fmt + clippy -D warnings + 531 sqlite tests + libsql
  build) after the one Rust string touched by the harness rewire; all JSON/TOML parse-checked.
