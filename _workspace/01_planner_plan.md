# WL-022 Implementation Plan: Streamable-HTTP MCP Transport

## Goal
Add a localhost-only HTTP JSON-RPC endpoint for remote MCP clients, with bearer token auth and dangerous-tools filtering.

## Current State
- MCP server is stdio-only (`weave mcp`)
- No HTTP transport
- No bearer auth
- All tools available regardless of transport

## Changes

### 1. MCP core refactor (`weave-mcp/src/mcp.rs`)
- Extract `dispatch_request(store, injector, nudge_template, request_json) -> response_json`
- Reuse in both stdio loop and HTTP handler
- Dangerous tools list: `weave_send`, `weave_ask`, `weave_answer`, `weave_ack`, `weave_schedule`, `weave_job_create`, `weave_job_update`, `weave_job_claim`, `weave_job_cancel`, `weave_claim_orchestrator`, `weave_setup`, `weave_uninstall`, `weave_review_add`, `weave_review_mark`, `weave_review_remove`, `weave_ask_permission`, `weave_permission_resolve`

### 2. HTTP server (`weave-mcp/src/http.rs`)
- `serve_http(store, injector, nudge_template, addr, token, dangerous)`
- `std::net::TcpListener` on loopback only
- Parse HTTP/1.1 POST with Content-Length
- Verify `Authorization: Bearer <token>`
- Call `dispatch_request`, return JSON-RPC response
- Without `--dangerous`, reject calls to mutating tools with JSON-RPC error

### 3. CLI (`weave/src/main.rs`)
- `weave serve --http <port> [--dangerous] [--token <token>]`
- Default port: 8787
- Default token: random 32-byte hex from `getrandom`, printed to stderr
- `--dangerous` enables mutating tools

### 4. Config (`weave-core/src/config.rs`)
- `http_port: Option<u16>`
- `http_token: Option<String>`
- `http_dangerous: bool`

### 5. Tests
- HTTP roundtrip test (start server, send JSON-RPC, verify response)
- Auth rejection test (missing/wrong bearer)
- Dangerous tool filtering test
