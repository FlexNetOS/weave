# Plan — WL-002: MCP daemon tools wiring (phased)

## Goal
Expose the optional presence daemon over MCP. The daemon writes periodic
heartbeats so peers show live status (complementing the existing TTL heuristic).

## Current gap
The `feat/presence-daemon` branch has a working daemon in the old single-crate
layout, but after the workspace split (WL-001) none of that code is on `master`.
The Store trait lacks `heartbeat`/`evict_stale_presence`, and there are no MCP
tools for daemon lifecycle.

## Phased approach (2 cycles remaining)

### Phase A — this cycle: Store layer + CLI daemon

1. **Schema / migration (both backends)**
   - New `presence` table: `name TEXT PRIMARY KEY, host TEXT, pid INTEGER,
     heartbeat_ts INTEGER`
   - Idempotent additive migration guarded by `PRAGMA user_version`.

2. **Store trait additions (`weave-core`)**
   - `heartbeat(name, host, pid) -> Result<()>` — upserts the row with
     `heartbeat_ts = now()`.
   - `evict_stale_presence(cutoff_secs: i64) -> Result<usize>` — deletes rows
     where `heartbeat_ts < now() - cutoff_secs`; returns count.
   - `peer_liveness(peer: &Peer) -> Liveness` — **default trait method** that
     tiers: (a) daemon heartbeat within 30s → `Live`, (b) `last_seen` within
     `ONLINE_TTL_SECS` (900s) → `Likely`, (c) else → `Offline`.

3. **CLI (`weave` bin)**
   - Add `DaemonCmd` enum: `start`, `stop`, `status`, `run`.
   - Port `handle_daemon` from `feat/presence-daemon` (argv-only `kill -0` /
     `kill -TERM`, PID file in `XDG_RUNTIME_DIR` or temp).
   - Daemon loop: every 15s heartbeat, every 60s evict.

4. **Tests**
   - Unit: schema migration roundtrip on both backends.
   - Unit: `heartbeat` + `evict_stale_presence` on both backends.
   - Unit: `peer_liveness` tier logic (mock Peer with varying timestamps).
   - Black-box CLI: `weave daemon start` spawns, `status` reports running,
     `stop` terminates, `status` reports stopped.

5. **Docs**
   - `CHANGELOG.md [Unreleased]` entry for daemon store + CLI.

### Phase B — next cycle: MCP tools + integration tests + docs

1. **MCP tools (`weave-mcp`)**
   - `weave_daemon_start` — idempotent start; returns pid.
   - `weave_daemon_stop` — SIGTERM + cleanup.
   - `weave_daemon_status` — running (pid + since) or stopped.

2. **Integration tests**
   - MCP tool advertisement includes the three daemon tools.
   - `weave_daemon_start` via MCP spawns process; `status` reflects it;
     `stop` terminates it.

3. **Docs**
   - `README.md` — daemon section.
   - `ARCHITECTURE.md` — presence/daemon layer.
   - `docs/TESTING.md` — daemon lifecycle test notes.

## Invariants in scope

- No shell (argv-only `kill -0` / `kill -TERM`)
- Parameterized SQL for presence table operations
- Layer DAG: store methods in `weave-core`, CLI in `weave` bin, MCP tools in
  `weave-mcp` (cannot reach up to bin logic)
- Input caps: daemon name uses existing `id_valid` / `check_ident`
- MCP stdout discipline: daemon tools only emit JSON-RPC on stdout
- No new default dependency

## Risks

- Daemon lifecycle tests can be flaky in parallel test runs (PID file collisions).
  Use temp-dir-scoped PID files in tests.
- Cross-platform `daemon_running`: Linux `/proc` check preferred over `kill -0`
  for accuracy, but `kill -0` is the portable fallback already used in the
  presence branch. Keep `kill -0` for portability.
