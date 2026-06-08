# WL-016 Implementation Plan: Scheduler / cron for messages

## Goal
Add one-shot and recurring scheduled message deliveries (`@daily`, `@hourly`, etc.) with SQLite-backed persistence and drift-safe execution. Scoped to a single cohesive cycle: schema + Store trait + both backends + CLI + MCP + a lightweight "tick" mechanism (checked on prompt hook + explicit `weave tick`). No background daemon thread.

## References
- `ARCHITECTURE.md` §2 (Store trait), §7 (invariants)
- `docs/TESTING.md`
- Skills: `weave-invariants`, `weave-test-discipline`

---

## 1. Schema Changes

### New table: `schedules`

```sql
CREATE TABLE IF NOT EXISTS schedules (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,            -- 'one_shot' | 'recurring'
    cron_expr   TEXT NOT NULL,            -- '@daily', '@hourly', or '0 9 * * 1-5'
    next_run    INTEGER NOT NULL,         -- UNIX epoch seconds of next execution
    sender      TEXT NOT NULL,
    recipient   TEXT NOT NULL,
    subject     TEXT,
    body        TEXT NOT NULL,
    created_ts  INTEGER NOT NULL,
    executed_ts INTEGER,                  -- set when a one-shot fires
    cancelled   INTEGER NOT NULL DEFAULT 0 -- soft-delete flag (0 | 1)
);
CREATE INDEX IF NOT EXISTS idx_schedules_next_run ON schedules(next_run);
CREATE INDEX IF NOT EXISTS idx_schedules_sender    ON schedules(sender);
```

**Rationale**: `AUTOINCREMENT` id is the user-visible schedule handle (simple integer, no minted prefix needed). `cancelled` is a soft flag so cancellation is non-destructive and auditable. `next_run` is the query key for the tick mechanism.

### Migrations (both backends)
- Add `schedules` table + indexes to the `SCHEMA` constant in `store.rs` and `store_libsql.rs`.
- Add `CREATE TABLE IF NOT EXISTS schedules (...)` to the `migrate()` function in both backends so legacy DBs gain the table on open.
- No `ALTER TABLE` needed (new table only).

---

## 2. Model Changes (`weave-core/src/model.rs`)

### New types

```rust
/// The lifecycle of a schedule row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    Pending,
    Executed,
    Cancelled,
}

/// One-shot vs recurring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    OneShot,
    Recurring,
}

impl ScheduleKind {
    pub fn as_str(self) -> &'static str { ... }
    pub fn from_str(s: &str) -> Result<Self, String> { ... }
}

/// A persisted scheduled message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: i64,
    pub kind: ScheduleKind,
    pub cron_expr: String,
    pub next_run: i64,
    pub sender: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub body: String,
    pub created_ts: i64,
    pub executed_ts: Option<i64>,
    pub cancelled: bool,
}
```

### Cron / preset evaluator (pure, no I/O, no new crate)

Add a minimal `cron` submodule in `model.rs` (or inline):

```rust
/// Supported presets and a restricted 5-field cron subset.
/// Presets: @hourly, @daily, @weekly, @monthly.
/// 5-field: `min hour day month dow` — integers, `*`, and `-` ranges only.
/// Returns the NEXT UNIX timestamp >= `after` for the given expression.
pub fn next_occurrence(cron_expr: &str, after: i64) -> Option<i64> { ... }
```

**Implementation strategy**: Parse presets to hard-coded tuples; for 5-field expressions, split on whitespace and brute-force scan forward from `after` in 60-second increments (capped at 366 days) — this is dependency-free, deterministic, and fast enough for the coarse granularity weave needs. No `chrono` / `cron` crate required.

**Validation**: `cron_valid(expr: &str) -> bool` — rejects empty, over-length (>64 chars), or metachar-bearing expressions before storage.

**Cap**: `MAX_CRON_EXPR_LEN = 64`.

---

## 3. Store Trait Changes (`weave-core/src/store.rs`)

Add to the `Store` trait (after the `presence` methods, before the closing brace):

```rust
/// WL-016: schedule a future message delivery.
/// Validates sender/recipient identities and body length via `check_ident`/`check_body`.
/// `next_run` must be in the future (>= now() allowed; the tick uses `<= now`).
/// Returns the new schedule id.
#[allow(dead_code)]
fn schedule_message(
    &self,
    sender: &str,
    recipient: &str,
    subject: Option<&str>,
    body: &str,
    kind: ScheduleKind,
    cron_expr: &str,
    next_run: i64,
) -> Result<i64>;

/// WL-016: list schedules created by `sender`, newest-first by `created_ts`,
/// capped at `clamp_limit(limit)`. Excludes cancelled rows by default?
/// Actually include them so the user sees the full state; the UI can filter.
#[allow(dead_code)]
fn list_schedules(&self, sender: &str, limit: i64) -> Result<Vec<Schedule>>;

/// WL-016: soft-cancel a schedule by id. Returns `true` if the row existed and
/// was pending (and is now cancelled). Idempotent: cancelling an already-
/// cancelled or executed row returns `false` without error.
#[allow(dead_code)]
fn cancel_schedule(&self, id: i64) -> Result<bool>;

/// WL-016: fetch schedules whose `next_run <= before_ts` AND `cancelled = 0`
/// AND (`executed_ts IS NULL` OR `kind = 'recurring'`), oldest-first by `next_run`.
/// The tick calls this with `before_ts = now()`.
#[allow(dead_code)]
fn get_due_schedules(&self, before_ts: i64) -> Result<Vec<Schedule>>;

/// WL-016: advance a schedule after execution.
/// - OneShot: sets `executed_ts = now()`.
/// - Recurring: computes the next occurrence via `model::next_occurrence` and
///   updates `next_run`; if no next occurrence is computable (malformed cron),
///   soft-cancels instead.
#[allow(dead_code)]
fn mark_schedule_executed(&self, id: i64) -> Result<()>;
```

**Default impl**: none needed; both backends implement directly.

---

## 4. Backend Implementation

### SqliteStore (`store.rs`)

- `schedule_message`: `INSERT INTO schedules (...) VALUES (...)` inside a transaction? Not needed (single row). Bind all fields with `params!`. Return `conn.last_insert_rowid()`.
- `list_schedules`: `SELECT * FROM schedules WHERE sender = ? ORDER BY created_ts DESC LIMIT ?`
- `cancel_schedule`: `UPDATE schedules SET cancelled = 1 WHERE id = ? AND cancelled = 0 AND executed_ts IS NULL` → check `changes() == 1`.
- `get_due_schedules`: `SELECT * FROM schedules WHERE next_run <= ? AND cancelled = 0 AND (executed_ts IS NULL OR kind = 'recurring') ORDER BY next_run ASC`
- `mark_schedule_executed`: Load row by id. If `OneShot`, `UPDATE schedules SET executed_ts = ? WHERE id = ?`. If `Recurring`, compute `next = next_occurrence(&cron_expr, now())`; if `Some(ts)`, `UPDATE next_run = ts`; else `UPDATE cancelled = 1`.

### LibsqlStore (`store_libsql.rs`)
Mirror the sqlite implementation statement-for-statement, using libsql's `conn.execute`/`query` + `block_on`. Use the same SQL text so both backends are byte-identical except for the driver API.

---

## 5. CLI Changes (`weave/src/main.rs`)

### New top-level commands in `Cmd` enum

```rust
/// Schedule a future message delivery.
Schedule {
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: String,
    #[arg(long, allow_hyphen_values = true)]
    subject: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    body: String,
    /// One-shot: absolute UNIX timestamp or RFC3339-ish? Keep it simple: --at <unix_seconds>.
    #[arg(long)]
    at: Option<i64>,
    /// Recurring: cron preset or expression.
    #[arg(long)]
    every: Option<String>,
},
/// List your scheduled messages.
Schedules {
    #[arg(long)]
    me: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: i64,
    #[arg(long)]
    json: bool,
},
/// Cancel a scheduled message.
CancelSchedule {
    #[arg(long)]
    id: i64,
},
/// Execute any due scheduled messages now (explicit tick).
Tick {
    #[arg(long)]
    me: Option<String>,
    /// also evaluate schedules for other senders (admin/debug); default self-only
    #[arg(long)]
    all: bool,
},
```

**Validation in handler**:
- `Schedule`: exactly one of `--at` or `--every` must be present (mutually exclusive).
- `--at` must be `>= now()` (or silently accept past values — the tick will fire them immediately).
- `--every` must pass `model::cron_valid`.
- `body` length checked by `store::check_body` (inherited automatically via `schedule_message`).

**Dispatch** (`match cmd` in `main`):
- `Cmd::Schedule { ... }` → resolve identity, parse kind, compute `next_run` (one-shot = `at`; recurring = `next_occurrence(every, now())`), call `store.schedule_message(...)`, print `Scheduled #{id} ({kind}) for {recipient} at {fmt_ts(next_run)}`.
- `Cmd::Schedules { ... }` → call `store.list_schedules(&me, limit)`, print human table or JSON.
- `Cmd::CancelSchedule { id }` → call `store.cancel_schedule(id)`, print success/failure.
- `Cmd::Tick { ... }` → call `execute_tick(store, &me, all)` (see §6).

---

## 6. Scheduler Execution (Tick Mechanism)

### Design constraint
weave has no daemon thread. The scheduler must be **daemon-free**, evaluated either:
1. **Implicitly** on every `weave hook prompt` (best-effort, after inbox drain)
2. **Explicitly** via `weave tick` (manual/scripted)

### `execute_tick(store, me, all)` function (in `main.rs`)

```rust
fn execute_tick(store: &dyn Store, me: &str, all: bool) -> Result<()> {
    let due = store.get_due_schedules(model::now())?;
    let mut fired = 0;
    for sched in due {
        // Self-only unless --all
        if !all && sched.sender != me {
            continue;
        }
        // Fire: send the message
        let mid = store.send(&sched.sender, &sched.recipient, sched.subject.as_deref(), &sched.body)?;
        // Advance the schedule row
        store.mark_schedule_executed(sched.id)?;
        // P6 delivery trace (best-effort)
        record_delivery_best_effort(store, mid, ..., &sched.recipient, ...);
        fired += 1;
    }
    println!("Tick: {fired} schedule(s) fired.");
    Ok(())
}
```

### Hook integration
In `handle_hook(...)` under the `"prompt"` arm, **after** the inbox drain and open-ask nudges:

```rust
// WL-016: evaluate due schedules on every prompt (best-effort)
if let Err(e) = execute_tick(store, &me, false) {
    eprintln!("[weave] schedule tick skipped (non-fatal): {e}");
}
```

This ensures schedules fire roughly once per user turn without any background process. The `mark_schedule_executed` update prevents double-fire even if the hook runs twice rapidly (transactional UPDATE + the one-shot `executed_ts` guard).

### Drift safety
- A schedule row is never hard-deleted; `cancelled` and `executed_ts` provide idempotency.
- `get_due_schedules` only returns rows where `next_run <= now` AND the row is not cancelled/executed.
- `mark_schedule_executed` updates the row atomically; if the same schedule is seen again in the same second, it will have been advanced/closed.
- For recurring schedules, `next_occurrence` is computed from the *current* `now()`, not from `next_run`, so clock jumps or missed ticks do not queue up unbounded backlogged fires — a missed daily schedule fires once and advances to the next future slot.

---

## 7. MCP Changes (`weave-mcp/src/mcp.rs`)

### New tools (add to `tools()` JSON array and `call_tool` match)

```json
{
    "name": "weave_schedule",
    "description": "Schedule a future message delivery. Exactly one of 'at' (one-shot unix timestamp) or 'every' (cron preset/expression) must be provided.",
    "inputSchema": {"type":"object","properties":{
        "from":{"type":"string"},
        "to":{"type":"string"},
        "subject":{"type":"string"},
        "body":{"type":"string"},
        "at":{"type":"integer","description":"One-shot: absolute UNIX timestamp."},
        "every":{"type":"string","description":"Recurring: @daily, @hourly, @weekly, @monthly, or a 5-field cron expression."}
    },"required":["to","body"]}
}
```

```json
{
    "name": "weave_schedules",
    "description": "List scheduled messages created by the caller.",
    "inputSchema": {"type":"object","properties":{
        "me":{"type":"string"},
        "limit":{"type":"integer"}
    },"required":[]}
}
```

```json
{
    "name": "weave_cancel_schedule",
    "description": "Cancel a scheduled message by its id. Soft-cancel (non-destructive).",
    "inputSchema": {"type":"object","properties":{
        "id":{"type":"integer"}
    },"required":["id"]}
}
```

```json
{
    "name": "weave_tick",
    "description": "Execute any due scheduled messages now. Self-only by default.",
    "inputSchema": {"type":"object","properties":{
        "me":{"type":"string"},
        "all":{"type":"boolean","description":"Evaluate schedules for all senders (admin/debug)."}
    },"required":[]}
}
```

### Tool handlers
- `tool_schedule`: validate `at` xor `every`, bound identities/subject/body, compute `next_run`, call `store.schedule_message`, return confirmation text.
- `tool_schedules`: call `store.list_schedules`, return human list or JSON.
- `tool_cancel_schedule`: call `store.cancel_schedule`, return success/failure.
- `tool_tick`: call `execute_tick` equivalent (or inline the logic), return count of fired schedules.

**Invariants**: All input caps enforced (`bound_ident`, `bound_subject`, `check_body` via store). No stdout except JSON-RPC frames.

---

## 8. Test Plan

### Unit tests (in `weave-core/src/model.rs` `#[cfg(test)]`)
- `next_occurrence_presets`: assert `@daily` from 2024-01-01 00:00:00 → next day 00:00:00; `@hourly` → next hour 00:00; etc.
- `next_occurrence_cron_ranges`: assert `0 9 * * 1-5` yields next weekday 09:00.
- `next_occurrence_no_past`: a missed daily schedule advances to the *next* future day, not the past.
- `cron_valid_rejections`: empty, over-length, control chars, unsupported 6-field expressions rejected.
- `schedule_kind_roundtrip`: `as_str`/`from_str` totality.

### Store unit tests (in `weave-core/src/store.rs` `#[cfg(test)]`)
- `schedule_one_shot_roundtrip`: create → list → assert fields.
- `schedule_cancel`: cancel → list no longer shows it as pending; `cancel_schedule` returns true, second cancel returns false.
- `schedule_due_query`: create a schedule with `next_run` in the past → `get_due_schedules(now)` returns it.
- `schedule_mark_executed`: one-shot → `mark_schedule_executed` → `executed_ts` set; recurring → `next_run` advanced.
- `schedule_double_fire_prevented`: call `mark_schedule_executed` twice; second call is a no-op (or harmless).

### Libsql mirror tests (in `weave-core/src/store_libsql.rs` `#[cfg(test)]`)
- Same assertions as the sqlite store tests, run when `--features libsql` is compiled.

### Integration tests (`tests/integration.rs`)
- `cli_schedule_one_shot`: `weave schedule --to bob --body hello --at <future_ts>` → expect `Scheduled #N`.
- `cli_schedules_list`: create two schedules → `weave schedules` → expect both in output.
- `cli_cancel_schedule`: create → cancel → `weave schedules` → expect cancelled state or exclusion.
- `cli_tick_fires_due`: create schedule with `next_run` in past → `weave tick` → expect message in bob's inbox.
- `mcp_schedule_tool`: `McpServer` test calling `weave_schedule` with `every: "@daily"` → expect success.
- `mcp_cancel_schedule_tool`: create via MCP, cancel via MCP, assert state.
- `mcp_tick_tool`: create past-due schedule, call `weave_tick`, assert fired.

### Security tests (`tests/security.rs`)
- `schedule_caps_reject_oversized_body`: body > 65536 bytes rejected at store layer.
- `schedule_caps_reject_long_cron`: cron expression > 64 chars rejected.
- `schedule_caps_reject_bad_identity`: sender/recipient with control chars rejected.
- `schedule_no_shell`: verify no new `Command::new("sh")` introduced (grep-based assertion, following existing security test pattern).

### Dual-backend verification
Run the full gate on both backends:
```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build --no-default-features --features libsql
cargo test --no-default-features --features libsql
```

---

## 9. Invariants to Verify

Per the `weave-invariants` skill, verify before finishing:

| # | Invariant | How verified in WL-016 |
|---|-----------|------------------------|
| 1 | **No shell, ever** | Tick uses `store.send` (pure DB operation). No `Command::new` added in scheduler code. |
| 2 | **Parameterize all SQL** | All schedule queries use `params!` placeholders. `cron_expr` is validated before storage but still bound as a parameter. |
| 3 | **Acyclic module layering** | New cron logic is pure (in `model`). `store` depends on `model`. `main`/`mcp` depend on `store` + `model`. No upward edges. |
| 4 | **Paste-safe injection** | Not applicable — scheduler does not introduce new injector paths; it reuses existing `store.send`. |
| 5 | **Input caps** | `MAX_BODY` (65536) enforced by `check_body`. `MAX_IDENT` enforced by `check_ident`. New `MAX_CRON_EXPR_LEN = 64` enforced at CLI/MCP boundary and store seam. `MAX_SUBJECT_LEN` inherited. |
| 6 | **Gate destructive ops** | `cancel_schedule` is a soft flag (non-destructive), so no `confirm` gate required. `gc` already handles hard deletion; schedules are included in `gc` pruning of old rows? **Not by default** — add `schedules` to `gc()`? Decision: yes, `gc` should prune `schedules WHERE created_ts < cutoff AND (cancelled = 1 OR executed_ts IS NOT NULL)` so stale rows don't accumulate. This is non-destructive (already terminal rows only). |
| 7 | **MCP stdout discipline** | Tick output in MCP returns as JSON-RPC result text, never a stray `println!`. Logs use `eprintln!`. |
| 8 | **No new heavyweight default dependency** | Cron evaluation is hand-rolled in `model.rs`. No `chrono`, `cron`, or `tokio` added to default dependencies. |

---

## 10. Open Questions / Decisions

1. **GC integration**: Should `Store::gc` prune old executed/cancelled schedule rows? **Recommendation**: Yes — add `DELETE FROM schedules WHERE created_ts < ? AND (cancelled = 1 OR executed_ts IS NOT NULL)` to both backends' `gc()` implementations. This keeps the table bounded without a separate sweeper.

2. **Broadcast schedules**: Should `schedule_message` allow broadcast recipients? **Recommendation**: Yes, reuse `is_broadcast` check from `send`. The tick path calls `store.send` which already handles broadcast semantics. No extra work.

3. **Time zones**: All timestamps are UNIX seconds (UTC). The cron evaluator operates in UTC. If a user wants local-time scheduling, that is out of scope for v0.1; document that `@daily` means 00:00 UTC.

4. **Recurring drift**: If a machine is asleep for 3 days, `@daily` will fire once and jump to the next future day (no catch-up burst). This is the desired daemon-free behavior — document it.

---

## 11. Files to Touch

| File | Change |
|------|--------|
| `weave-core/src/model.rs` | Add `Schedule`, `ScheduleKind`, `ScheduleState`, `next_occurrence`, `cron_valid`, `MAX_CRON_EXPR_LEN` |
| `weave-core/src/store.rs` | Add `schedule_message`, `list_schedules`, `cancel_schedule`, `get_due_schedules`, `mark_schedule_executed` to trait; implement for `SqliteStore`; add `schedules` to `SCHEMA` and `migrate()`; add `schedules` pruning to `gc()` |
| `weave-core/src/store_libsql.rs` | Mirror all store.rs schedule changes for `LibsqlStore` |
| `weave/src/main.rs` | Add `Schedule`, `Schedules`, `CancelSchedule`, `Tick` to `Cmd`; add dispatch arms; add `execute_tick` helper; wire tick into `handle_hook` prompt arm |
| `weave-mcp/src/mcp.rs` | Add 4 tools to `tools()` and `call_tool`; implement handlers |
| `tests/integration.rs` | Add CLI + MCP schedule roundtrip tests |
| `tests/security.rs` | Add cap/rejection tests for schedule inputs |

---

## 12. Single-Cycle Scope Confirmation

This plan is scoped to fit one cohesive cycle:
- **Does NOT add** a background daemon thread.
- **Does NOT add** a new date/crate dependency.
- **Does NOT implement** complex cron features (L, W, #, step values) — only presets + simple integer/range fields.
- **Does NOT add** schedule editing/modification — only create, list, cancel, tick.
- **Does add** full schema, both backends, CLI, MCP, hook integration, tests, and invariants verification.

**Estimated LOC**: ~600–800 lines across all files, well within a single-cycle budget.
