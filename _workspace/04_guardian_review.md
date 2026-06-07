# Guardian Review — WL-002 Phase A (daemon store + CLI)

Reviewer: guardian audit (invariants + drift-guard + docs sync)
Inputs: uncommitted diff, `_workspace/01_planner_plan.md`, `_workspace/02_implementer_changes.md`, `_workspace/03_verifier_report.md`

## Invariants

| File:line | Rule | Verdict |
|-----------|------|---------|
| `weave/src/main.rs` daemon code | No shell — `kill -0` / `kill -TERM` are argv-only `Command::new("kill")` | PASS |
| `weave-core/src/store.rs` / `store_libsql.rs` | Parameterized SQL (`params![]`) for presence table operations | PASS |
| All crates | Layer DAG intact: store in `weave-core`, CLI in `weave` bin | PASS |
| `weave/src/main.rs` | Paste-safe injection unchanged (no injector changes in this phase) | PASS |
| `weave/src/main.rs` | Input caps: `resolve_me` + `check_ident` used for daemon identity | PASS |
| `weave/src/main.rs` | Destructive ops: `stop` sends SIGTERM (process control, not data loss) | PASS |
| `weave-mcp/src/mcp.rs` | MCP stdout discipline: no changes in this phase | PASS |
| All Cargo.toml | No new default dependency added | PASS |

## Drift

| File | Category | Feeds build? | Verdict |
|------|----------|--------------|---------|
| All new/modified files | — | — | No drift — all Rust-native |

No drift detected.

## Docs

| Doc | Verdict |
|-----|---------|
| `CHANGELOG.md` | PASS — Phase A entry present |
| `README.md` / `ARCHITECTURE.md` | Deferred to Phase B (acknowledged in plan) |

## Verdict

APPROVE

Phase A preserves every invariant, introduces no drift, and passes the full
dual-backend gate. The `presence` table migration is additive and guarded.
Ready for delivery. Phase B (MCP tools) remains for the next cycle.
