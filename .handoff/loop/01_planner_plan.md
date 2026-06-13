# WL-050 — MCP progressive-disclosure refactor (ADR-0003)

## Goal
Collapse the 73 eager flat `weave_*` MCP tools into a tiny standing surface (≤ ~2k tokens)
with **zero capability loss**, via a `weave` meta-tool that exposes the full operation set
on demand. Keep a backward-compatible eager-flat mode behind a flag.

## Architecture mapping (weave-mcp/src/mcp.rs only — no core/inject/store change)
- **`tools()` (line ~3113)** currently emits the full 73-tool `json!([...])` for `tools/list`.
  Split:
  - `fn tool_catalog() -> Vec<Value>` — the existing 73 defs (canonical schema source). Used by
    `describe`/`search`/`list` and by eager mode. **All ops preserved verbatim.**
  - `fn meta_tool_def() -> Value` — the `weave` meta-tool schema (mode discriminator).
  - `fn tools() -> Value` — STANDING surface: progressive default → `[meta_tool_def()]`;
    eager (`WEAVE_MCP_EAGER=1`/`true`) → the full catalog (today's behavior, backward-compat).
- **`call_tool` (line ~404)** gains a `"weave" => tool_meta(...)` arm. Thread a `dangerous: bool`
  param through `call_tool` (2 call sites: `dispatch_request` passes its flag; test helper passes
  `true`) so the meta `call` mode preserves the safe-HTTP dangerous gate on the *inner* op.

## Meta-tool semantics (`weave`)
inputSchema: `{ mode: enum[search,describe,call,list], query?, name?, arguments? }`, required `[mode]`.
- `search {query, limit?}` → case-insensitive substring over name+description → `{count, matches:[{name,summary}]}`.
- `list` → all op names (the index), grouped by `weave_<ns>` prefix.
- `describe {name}` → full `{name,description,inputSchema}` for the op (accepts with/without `weave_` prefix). Unknown → error.
- `call {name, arguments}` → dispatch to `call_tool(...)`. Guards:
  - recursion: inner `name == "weave"` → error.
  - safe-HTTP: `!dangerous && is_dangerous_tool(inner)` → same rejection as the flat path.

## Invariants in scope
- **MCP stdout discipline** — meta-tool returns text content only; logging stays stderr. (unchanged path)
- **Destructive-op gating preserved** — `call` re-applies `is_dangerous_tool` under `!dangerous`.
- **No new dep, no shell, no SQL** — pure dispatch/JSON refactor; default dep tree byte-identical.
- **Layer DAG** — change confined to `weave-mcp`; no upward dep, no core/inject edit.

## Test layers (weave-test-discipline → McpServer/dispatch tests in mcp.rs `#[cfg(test)]`)
1. progressive default `tools/list` → exactly the meta-tool (not 73).
2. eager mode (`WEAVE_MCP_EAGER=1`) `tools/list` → full catalog count == catalog len.
3. meta `search "inbox"` → contains `weave_inbox`.
4. meta `describe weave_send` → inputSchema present + required `[to,body]`; unknown name → error.
5. meta `call weave_peers` → equals direct `call_tool("weave_peers")`.
6. meta `call` recursion guard (`name:"weave"`) → error.
7. meta `call` safe-HTTP gate: `dangerous=false` + inner dangerous op → rejected (parity with flat).
8. catalog↔dispatch completeness: every `tool_catalog()` name is dispatchable (no orphan/no drift).

## Docs to sync
- `ARCHITECTURE.md` (MCP surface section) — note progressive disclosure + eager flag.
- `CLAUDE.md` "Where things live" already points MCP → mcp.rs; add the flag mention.
- `CHANGELOG.md [Unreleased]`.
- Reference ADR-0003.

## Out of scope (noted, not this PR)
- The 14 namespaced dispatcher tools (`weave_msg` …) — ADR says "and/or"; the meta-tool alone
  meets ≤2k standing tokens + zero loss. Optional WL-050 follow-on if the owner wants them.
- WL-051 (token-light invariant + guardian budget gate) is the sibling card.

## Dual-backend
No `Store` touch → both backends compile unchanged; still run the full gate on sqlite + libsql.
