# weave-loop handoff

## Current session state
- **Branch:** `feat/zellij-pane-targeting`
- **Worktree:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Last completed:** WL-015 — Structured question types
- **Cycles this session:** 2 / 3 (one cycle remaining before mandatory handoff)
- **Commit:** `696b4ca`

## What was done (this session)

### Cycle 1 — WL-014: Reminder injection for open asks
- Added `Store::has_open_asks()` to both sqlite and libsql backends
- Prompt-hook nudge injection for open asks on drain (content-free nudge)
- 2 integration tests + 2 unit tests per backend
- Full gate green on both backends
- Committed: `32c961b`

### Cycle 2 — WL-015: Structured question types
- Added `AskKind` enum (`FreeText`, `Choice`, `ToolPermission`) with `as_str`/`from_str`/`parse`/`default`
- Added `kind` (TEXT NOT NULL DEFAULT 'free_text') and `options` (TEXT) columns to `asks` table
- Schema migrations in both sqlite and libsql backends
- Extended `Store::ask()` signature with `kind: AskKind` and `options: Option<&str>`
- Updated CLI (`main.rs` `Cmd::Ask`), MCP (`mcp.rs` `tool_ask`), and all test call sites
- Fixed `list_asks` projection in both backends to include `kind, options`
- Added `#[allow(clippy::too_many_arguments)]` to `Store::ask`
- Full gate green on both backends (fmt + clippy -D warnings + test)
- Committed: `696b4ca`

## Next up
WL-016: Scheduler / cron for messages — one-shot and recurring scheduled deliveries with SQLite-backed persistence and drift-safe execution. This is the next uncompleted item in the backlog.

## Known issues / blockers
None. Both backends build, test, and lint clean.

## How to resume
```bash
cd /home/drdave/Desktop/meta/weave-mcp-daemon-tools
git status  # should be clean on branch feat/zellij-pane-targeting
cargo test --all-targets
cargo test --all-targets --no-default-features --features libsql
```
