# weave-loop handoff

## Current session state
- **Branch:** `feat/zellij-pane-targeting`
- **Worktree:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Last completed:** WL-017 — Mesh memory system
- **Cycles this session:** 1 / 3
- **Commit:** `9998190`

## What was done (this session)

### Cycle 1 — WL-017: Mesh memory system
- Added `weave-core/src/memory.rs` with `MemoryScope` (Global/Project/Persona/Orchestrator), `MemoryEntry`, full CRUD + search API
- Added `build_context_prefix` for automatic memory context prefixing on message delivery
- Added `config_dir()` helper in `config.rs`
- CLI: `weave memory write/read/search/list/delete/scopes` + `--no-memory` opt-out on ask/send/reply
- MCP: 5 memory tools (`weave_memory_write/read/search/list/delete`) with `no_memory` support
- Context prefixing hooked into `tool_ask`, `tool_send`, `tool_reply`, `tool_answer`
- Input caps: key ≤128 [a-zA-Z0-9_-], title ≤256, tags ≤16×64, body ≤64KiB, files ≤10k/scope, prefix ≤5 entries
- Path traversal defense via `sanitize_key`
- Tests: 13 unit tests in memory.rs, integration tests (CLI + MCP), security tests (path traversal + caps)
- Full gate green: **463 passed** (sqlite), **433 passed + 1 ignored** (libsql)
- Committed: `9998190`

## Next up
WL-018: Birth certificates / runtime identity envelopes — mint unguessable nonces at `SessionStart` to prevent path-based identity takeover during lazy MCP registration (repowire parity).

## Known issues / blockers
None. Both backends build, test, and lint clean.

## How to resume
```bash
cd /home/drdave/Desktop/meta/weave-mcp-daemon-tools
git status  # should be clean on branch feat/zellij-pane-targeting
cargo test --all-targets
cargo test --all-targets --no-default-features --features libsql
```
