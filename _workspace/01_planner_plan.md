# WL-021 Implementation Plan: PreToolUse Tool Approval

## Goal
Gate operations behind blocking approval questions using the existing ToolPermission ask kind (WL-015), with timeout-based auto-deny.

## Current State
- `AskKind::ToolPermission` exists with `options` field for `tool_name\ntool_args`
- No permission-specific CLI, MCP tools, or timeout handling

## Changes

### 1. Model (`model.rs`)
- New `PermissionStatus` enum: `Pending`, `Approved`, `Denied`, `Timeout`
- New `PERMISSION_TIMEOUT_SECS: i64 = 300` (5 min default)
- New `permission_status(ask: &Ask, timeout_secs: i64) -> PermissionStatus` helper
  - Open + within timeout → Pending
  - Open + expired → Timeout
  - Answered/Acked + body == "approve" → Approved
  - Answered/Acked + body != "approve" → Denied

### 2. Store trait + both backends
- `permission_verdict(&self, correlation_id: &str, timeout_secs: i64) -> Result<PermissionStatus>`
  - Reads ask via `get_ask`, reads answer message body if present
  - Delegates to `permission_status` helper
- `list_permissions(&self, me: &str, filter: PermissionStatus, limit: i64) -> Result<Vec<Ask>>`
  - Lists ToolPermission asks where `me` is asker or askee

### 3. CLI (`main.rs`)
- `weave ask permission <to> --tool <tool> [--args <args>]` — creates ToolPermission ask
- `weave permission status <ask_id>` — shows verdict
- `weave permission list [--filter pending|approved|denied|timeout] [--limit N]` — lists permissions

### 4. MCP (`mcp.rs`)
- `weave_ask_permission(to, tool, args?, from?)` — creates ToolPermission ask
- `weave_permission_status(id)` — returns verdict
- `weave_permission_list(filter?, limit?, me?)` — lists permissions

### 5. Tests
- Model unit tests for `permission_status` logic
- Store roundtrip tests for both backends
- CLI integration tests
- MCP tool tests
