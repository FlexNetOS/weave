# WL-050 implementer change log

## weave-mcp/src/mcp.rs (only source file changed — no core/inject/store touch)
- Split `fn tools() -> Value` → `fn tool_catalog() -> Vec<Value>` (canonical 73-op registry; defs verbatim).
- Added `fn meta_tool_def() -> Value` — the `weave` meta-tool schema (mode: search/describe/call/list).
- New `fn tools() -> Value`: progressive default → `[meta_tool_def()]`; `WEAVE_MCP_EAGER=1` → full catalog.
- Added `fn eager_mode()`, `fn first_sentence()`, `fn normalize_op_name()` (bare name ⇒ `weave_` prefix).
- Added `fn tool_meta(...)` — implements search/list/describe/call. `call` re-applies the safe-HTTP
  dangerous gate to the INNER op, refuses self-recursion (`name=="weave"`), routes via `call_tool`.
- `call_tool` gained a `dangerous: bool` param + a `"weave" => tool_meta(...)` arm. 2 call sites updated
  (dispatch_request passes its flag; test helper passes true).

## weave/tests/common/mod.rs
- `McpServer::spawn_full` now sets `WEAVE_MCP_EAGER=1` by default (overridable via extra_env), so the
  historical "tool advertised in tools/list" integration assertions verify the eager-flat compat path.

## Docs
- ARCHITECTURE.md (`mcp.rs` section + roadmap bullet), CHANGELOG.md [Unreleased].

## Invariants preserved
- No new dep / no shell / no SQL. Default `cargo tree` unchanged (118-line fingerprint).
- MCP stdout discipline untouched. Destructive-op gate preserved (re-applied on inner op).
- Layer DAG: change confined to weave-mcp; no upward dep.
