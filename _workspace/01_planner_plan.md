# Plan — WL-011: Optional weaved presence daemon

## Goal
Verify whether WL-011 is already covered by prior work and update the backlog
accordingly.

## Investigation
- WL-002 Phase A (commit 2f1e753) implemented:
  - `presence` table with heartbeat/eviction
  - `Store` trait methods: `heartbeat`, `evict_stale_presence`, `peer_liveness`
  - CLI: `weave daemon start|stop|status|run`
- WL-002 Phase B (commit 2f1e753 / PR #43) implemented:
  - MCP tools: `weave_daemon_start`, `weave_daemon_stop`, `weave_daemon_status`
- The daemon is **optional** — weave works without it (hook-driven TTL fallback).
- "Lifecycle eviction" is implemented as time-based eviction in the daemon loop
  (`evict_stale_presence` with a 30 s cutoff).

## Conclusion
WL-011 is a **duplicate** of WL-002. No code changes required.

## Changes
- `backlog.md` — flip WL-011 to `- [x]` with a duplicate note.
- `TASKS.md` — flip the M3 daemon line to `- [x]` referencing WL-002.

## Verify
- Run the full gate to confirm zero drift.
