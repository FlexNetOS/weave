# weave-loop handoff

## Current session state
- **Branch:** `feat/zellij-pane-targeting`
- **Worktree:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Last completed:** WL-016 — Scheduler / cron for messages
- **Cycles this session:** 3 / 3 (budget exhausted — handoff required)
- **Commit:** `558d126`

## What was done (this session)

### Cycle 1 — WL-014: Reminder injection for open asks
- Added `Store::has_open_asks()` to both backends
- Prompt-hook nudge injection for open asks on drain
- Full gate green on both backends
- Committed: `32c961b`

### Cycle 2 — WL-015: Structured question types
- Added `AskKind` enum (FreeText/Choice/ToolPermission)
- Added `kind` and `options` columns to `asks` table (both backends)
- Extended `Store::ask()` signature
- Updated CLI, MCP, and all test call sites
- Full gate green on both backends
- Committed: `696b4ca`

### Cycle 3 — WL-016: Scheduler / cron for messages
- Added `schedules` table with indexes (both backends)
- Added `Schedule`, `ScheduleKind`, `ScheduleState` types
- Pure cron evaluator: presets `@hourly`/`@daily`/`@weekly`/`@monthly` + 5-field subset, no new deps
- Added 5 Store methods: `schedule_message`, `list_schedules`, `cancel_schedule`, `get_due_schedules`, `mark_schedule_executed`
- CLI: `weave schedule`, `weave schedules`, `weave cancel-schedule`, `weave tick`
- MCP tools: `weave_schedule`, `weave_schedules`, `weave_cancel_schedule`, `weave_tick`
- Tick wired into `prompt` hook (after drain + ask nudge)
- GC prunes stale executed/cancelled schedule rows
- Tests: unit (cron), store (both backends), integration (CLI + MCP), security (caps)
- Full gate green: **442 passed** (sqlite), **412 passed + 1 ignored** (libsql)
- Committed: `558d126`

## Next up
WL-017: Mesh memory system — filesystem-backed scoped memory under `~/.config/weave/memory/` (global, project, persona, orchestrator) with CLI read/write/search and automatic context prefixing on ask delivery (repowire parity).

## Known issues / blockers
None. Both backends build, test, and lint clean.

## How to resume
```bash
cd /home/drdave/Desktop/meta/weave-mcp-daemon-tools
git status  # should be clean on branch feat/zellij-pane-targeting
cargo test --all-targets
cargo test --all-targets --no-default-features --features libsql
```
