# Guardian Review — WL-002 Phase B (MCP daemon tools)

Reviewer: guardian audit (invariants + drift-guard + docs sync)
Inputs: uncommitted diff for Phase B, `_workspace/01_planner_plan.md`, Phase B implementer changes, verification report.

## Invariants

| File:line | Rule | Verdict |
|-----------|------|---------|
| `weave-mcp/src/mcp.rs` daemon tools | No shell — `kill -0` / `kill -TERM` are argv-only `Command::new("kill")` | PASS |
| `weave-mcp/src/mcp.rs` | MCP stdout discipline: JSON-RPC responses only; logs via `eprintln!` | PASS |
| `weave-mcp/src/mcp.rs` | Layering: pidfile logic duplicated in `weave-mcp`; no upward dep on `weave` bin | PASS |
| All crates | No new default dependency | PASS |

## Drift

No drift — all changes are Rust-native.

## Docs

| Doc | Verdict |
|-----|---------|
| `README.md` | PASS — new "Presence daemon" subsection |
| `ARCHITECTURE.md` | PASS — daemon/presence section added |
| `docs/TESTING.md` | PASS — daemon lifecycle coverage noted |
| `CHANGELOG.md` | PASS — Phase B entry present |

## Verdict

APPROVE

Phase B preserves all invariants, introduces no drift, and passes the full
dual-backend gate. Ready for delivery.
