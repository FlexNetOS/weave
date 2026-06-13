# WL-050 guardian review — MCP progressive disclosure (ADR-0003)

Branch `wl-050-mcp-progressive` (uncommitted worktree changes) vs origin/develop @ 1a9bc1f.
Read-only audit. Files: `weave-mcp/src/mcp.rs`, `weave/tests/common/mod.rs`, `weave/tests/integration.rs`, `ARCHITECTURE.md`, `CHANGELOG.md`.

## 1. Security / correctness invariants
- **No shell** — OK. Diff adds no `Command`/`sh -c`/`format!`-built command. Pure dispatch+JSON. (mcp.rs)
- **Parameterized SQL** — OK (N/A). weave-mcp holds no SQL; SQL lives in weave-core/store, untouched. No string-interp SQL introduced.
- **Layer DAG intact** — OK. Change confined to `weave-mcp` + `weave` tests; `weave-core`/`weave-inject` untouched; no new `use` imports → no upward dep. (compiler-enforced; verified)
- **Paste-safe injection** — OK (N/A). No mux/injector arm changed; `injector` is threaded through unchanged.
- **Input caps** — OK. `mode=search` clamps `limit` to `1..=200` (mcp.rs:3842-3846). `call` routes to `call_tool`, which applies the same per-op caps as the flat path (no bypass).
- **Destructive-op gating** — OK. The key risk surface is clean:
  - Outer gate: `weave` meta-tool is NOT in `DANGEROUS_TOOLS` (correct — it is a gateway, and search/describe/list must work in safe mode). (mcp.rs:271-309)
  - Inner gate: `mode=call` re-applies `!dangerous && is_dangerous_tool(&want)` to the INNER op before dispatch, returning the byte-identical "disabled in safe HTTP mode" rejection. (mcp.rs:3901-3905)
  - Self-recursion guard: `mode=call name=="weave"` → rejected (mcp.rs:3896-3898).
  - Canonical routing: `call` dispatches via `call_tool(...)` (mcp.rs:3907-3917) — no reimplementation, so all per-tool guards/`confirm` checks (e.g. `weave_clear`) are preserved.
  - `dangerous` flag plumbed correctly: `dispatch_request` → `call_tool` → `tool_meta` → inner `call_tool` (mcp.rs:324, 384, 3832, 3916). HTTP safe mode passes `false`; the test helper passes `true`.
  - Verified by `meta_call_preserves_safe_http_gate` driving the full `dispatch_request` path with `dangerous=false` on `weave_clear` → blocked (mcp.rs:5660-5685, PASS).
- **MCP stdout discipline** — OK. `tool_meta` returns `Result<String,String>` text content via the existing `tools/call` content frame; no new stdout writes, no logging change.

## 2. Rust-native drift guard
- No manifest change: `Cargo.toml` (all crates) and `Cargo.lock` show zero diff → no new dependency. Default `cargo tree` unchanged.
- No non-Rust build/runtime intrusion: only `.rs` + `.md` files touched; no generated sidecar feeds the build.
- No new heavyweight dep; no shell; no SQL. Confirmed a pure dispatch/JSON refactor.

## 3. Docs sync
- **ARCHITECTURE.md** — OK. Roadmap bullet flips WL-050 to "done" with the meta-tool + `WEAVE_MCP_EAGER=1` fallback; new mcp.rs subsection describes `tool_catalog()` as canonical registry, the four modes, inner-op gate re-application, recursion refusal, and eager-flat compat. Matches code.
- **CHANGELOG.md [Unreleased]** — OK. Describes single meta-tool default surface, four modes, prefix-optional names, inner-op gate + recursion refusal, `WEAVE_MCP_EAGER=1`, and "no dependency change / cargo tree byte-identical / both backends green." Matches code.

## 4. Completeness / zero-loss
- Every catalog op reachable two ways:
  - **tools/call flat name** — unchanged dispatch (mcp.rs:363-395); only the standing `tools/list` advertisement changed.
  - **meta `call`** — routes through the same `call_tool`.
- `every_catalog_op_is_dispatchable` (mcp.rs:5692-5718) iterates `tool_catalog()` and asserts no entry hits the `Unknown tool:` catch-all (mcp.rs:504) — a real catalog↔dispatch drift guard. PASS.
- `eager_mode_restores_the_full_flat_table` asserts eager `tools/list` len == `tool_catalog().len()` (no op dropped). PASS.
- Independent re-run on default (sqlite) backend: progressive (1), meta_* (8), every_catalog_op_is_dispatchable (1), eager (1), integration `mcp_progressive_disclosure_default_surface_and_meta_roundtrip` (1) — all PASS. Verifier reports both backends green (sqlite 577, libsql 537; clippy clean on default/libsql/sign/obscura).

## Verdict

**APPROVE**

No BLOCK or WARN findings. The meta-tool `call` mode is not a destructive-op bypass (inner `is_dangerous_tool` re-check + self-recursion guard + canonical `call_tool` routing, proven end-to-end). Zero-loss is test-guarded in both directions. No Rust-native drift, no new dependency, docs match code. The leader may proceed to commit/handoff.
