//! libSQL / Turso backend (feature `libsql`).
//!
//! This mirrors [`crate::store::SqliteStore`] exactly — same schema, same SQL,
//! same per-reader read-tracking and broadcast semantics — but talks to the
//! Turso `libsql` client instead of bundled rusqlite. That client is async
//! (tokio), while the rest of weave is synchronous and the [`Store`] trait is
//! sync. We bridge the two by holding a private current-thread tokio runtime in
//! the store and `block_on`-ing every operation. The runtime is single-threaded
//! and owned exclusively by this store, so each call is serialized — matching
//! the single-connection model of `SqliteStore`.
//!
//! Backend selection:
//!   - `cfg.libsql_url == Some` → remote database (`Builder::new_remote`) with
//!     the optional auth token.
//!   - otherwise               → local file at `cfg.db_path()`
//!     (`Builder::new_local`), which produces a libSQL-compatible SQLite file.
//!
//! ## Build & backend selection — RESOLVED
//! `libsql` and `sqlite` are MUTUALLY EXCLUSIVE Cargo features. Both rusqlite's
//! `bundled` SQLite and libsql's `libsql-ffi` statically link their own SQLite C
//! core; enabling both at once collides at link time (duplicate `sqlite3_*`
//! symbols). The crate makes the SQLite backend optional (`sqlite` feature, on by
//! default) so the libSQL backend builds alone with exactly one SQLite:
//!
//! ```text
//! cargo build                                   # default: sqlite (rusqlite)
//! cargo build --no-default-features --features libsql   # libSQL/Turso only
//! ```
//!
//! A `compile_error!` in `main.rs` rejects enabling both features together. This
//! backend is verified to build, link, clippy-clean, and run (local-file mode:
//! send/inbox/read-tracking/broadcast/sessions all match the SQLite backend).
//!
//! Written against the real `libsql` 0.9 API (`Builder`, `Connection`, `Rows`,
//! `Row`, `Value`). NOTE: `PRAGMA journal_mode=WAL` returns a row, so it is issued
//! via `query`, not `execute` (libsql's `execute` rejects row-returning statements).

use crate::config::{Config, StoreSource, REMOTE_TIMEOUT_MS_DEFAULT};
use crate::model::{
    ask_id_valid, ask_many_id_valid, attempt_id_valid, classify_ask_many, is_broadcast,
    job_id_valid, new_ask_id, new_ask_many_id, new_attempt_id, new_job_id, new_review_id, now,
    permission_status, pr_url_valid, Ask, AskGroup, AskKind, AskManyChildView, AskManyResult,
    AskRole, AskState, BridgePlatform, BridgeRuntimeErrorUpdate, BridgeRuntimeState,
    BridgeRuntimeStatus, BridgeRuntimeUpdate, BridgeStagedEvent, ClaimOutcome, DeliveryTrace,
    Intent, Job, JobFilter, JobPatch, JobResultView, JobSpec, JobState, Lease, Message,
    OrchestratorStatus, Peer, PermissionStatus, ReviewItem, ReviewItemState, ReviewQueueFilter,
    Schedule, ScheduleKind, BROADCAST_SQL, MAX_CRON_EXPR_LEN, MAX_DELIVERY_ROWS,
    MAX_REVIEW_IDENT_LEN, MAX_REVIEW_TITLE_LEN,
};
use crate::store::{
    append_progress_event, canonical_source, check_birth_cert, check_body, check_host, check_ident,
    check_job_text, clamp_field, clamp_limit, is_alive, job_result_view, merge_peer_views,
    merge_session_views, mint_birth_cert, remote_scheme_host, reply_subject, sanitize_tag,
    store_label, validate_bridge_claim, validate_bridge_inbox_completion, validate_bridge_owner_id,
    validate_bridge_staging, validate_bridge_update, validate_job_patch, validate_job_spec,
    AskManyOutcome, Origin, PeerView, Pulled, RevocationEvent, RevocationKind, SessionInfo,
    SessionView, Store, VerifyPolicy, MAX_ASK_MANY_TARGETS, MAX_BRANCH_LEN, MAX_KEYS_PER_IDENT,
    MAX_PULL_PER_DRAIN, MAX_REPO_LEN, MAX_REVOCATIONS_LIST, MAX_SESSIONS, MAX_WORKTREE_LEN,
    PRESENCE_TTL_SECS,
};
use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database, OpenFlags, Value};
use tokio::runtime::Runtime;

/// True if `e`'s chain is a transient SQLite "database is locked" (SQLITE_BUSY).
///
/// libsql 0.9's `busy_timeout()` API (and even `PRAGMA busy_timeout`) is not
/// reliably honored for a *local* file, so under concurrent multi-process open —
/// every starting `weave` runs the idempotent open-time migrations — a writer can
/// still see an immediate lock instead of waiting. We retry such writes at the app
/// layer (mirrors the rusqlite backend's busy_timeout semantics).
fn is_db_locked(e: &anyhow::Error) -> bool {
    let mut s = String::new();
    for cause in e.chain() {
        s.push_str(&cause.to_string());
        s.push('\n');
    }
    let s = s.to_ascii_lowercase();
    s.contains("database is locked") || s.contains("database table is locked")
}

/// Same schema as `SqliteStore`. Executed statement-by-statement because the
/// libsql remote/HTTP path runs one statement per round-trip; `execute_batch`
/// works for local but splitting keeps both backends identical.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS messages (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        ts              INTEGER NOT NULL,
        sender          TEXT NOT NULL,
        recipient       TEXT NOT NULL,
        subject         TEXT,
        body            TEXT NOT NULL,
        in_reply_to     INTEGER,
        idempotency_key TEXT UNIQUE,
        trace_id        TEXT,
        priority        TEXT NOT NULL DEFAULT 'normal',
        superseded_by   INTEGER,
        expires_at      INTEGER,
        kind            TEXT,
        request_priority TEXT,
        request_ttl     INTEGER,
        request_supersedes INTEGER,
        request_dedup_idle INTEGER
    )",
    "CREATE TABLE IF NOT EXISTS reads (
        message_id INTEGER NOT NULL,
        reader     TEXT NOT NULL,
        ts         INTEGER NOT NULL,
        PRIMARY KEY (message_id, reader)
    )",
    "CREATE TABLE IF NOT EXISTS wake_acks (
        reader  TEXT PRIMARY KEY,
        last_id INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS peers (
        name        TEXT PRIMARY KEY,
        mux         TEXT NOT NULL,
        target      TEXT NOT NULL,
        socket      TEXT NOT NULL DEFAULT '',
        cwd         TEXT,
        last_seen   INTEGER NOT NULL,
        pid         INTEGER,
        host        TEXT NOT NULL DEFAULT '',
        repo        TEXT NOT NULL DEFAULT '',
        branch      TEXT NOT NULL DEFAULT '',
        worktree_id TEXT NOT NULL DEFAULT '',
        circle      TEXT NOT NULL DEFAULT 'default',
        role        TEXT NOT NULL DEFAULT 'peer',
        turn_state     TEXT NOT NULL DEFAULT '',
        description    TEXT NOT NULL DEFAULT '',
        description_ts INTEGER NOT NULL DEFAULT 0,
        birth_cert     TEXT,
        contact_policy TEXT NOT NULL DEFAULT 'open',
        client_session TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS outbox (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        ts              INTEGER NOT NULL,
        to_peer         TEXT NOT NULL,
        to_host         TEXT NOT NULL DEFAULT '',
        from_peer       TEXT NOT NULL,
        subject         TEXT,
        body            TEXT NOT NULL,
        sig             TEXT NOT NULL DEFAULT '',
        idempotency_key TEXT,
        trace_id        TEXT,
        priority        TEXT NOT NULL DEFAULT 'normal',
        ttl             INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS pull_cursor (
        source  TEXT PRIMARY KEY,
        last_id INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS keys (
        identity TEXT PRIMARY KEY,
        pubkey   TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS identity_keys (
        identity TEXT NOT NULL,
        pubkey   TEXT NOT NULL,
        added_ts INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (identity, pubkey)
    )",
    "CREATE TABLE IF NOT EXISTS revocations (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        ts        INTEGER NOT NULL,
        fp        TEXT NOT NULL,
        identity  TEXT NOT NULL DEFAULT '',
        source    TEXT NOT NULL DEFAULT '',
        kind      TEXT NOT NULL DEFAULT 'enforced'
    )",
    "CREATE TABLE IF NOT EXISTS asks (
        id              TEXT PRIMARY KEY,
        question_msg_id INTEGER NOT NULL,
        answer_msg_id   INTEGER,
        asker           TEXT NOT NULL,
        askee           TEXT NOT NULL,
        subject         TEXT,
        request_subject TEXT,
        request_subject_provided INTEGER,
        state           TEXT NOT NULL,
        kind            TEXT NOT NULL DEFAULT 'free_text',
        options         TEXT,
        reply_to        TEXT,
        close_note      TEXT,
        opened_ts       INTEGER NOT NULL,
        updated_ts      INTEGER NOT NULL,
        closed_ts       INTEGER,
        parent_id       TEXT
    )",
    "CREATE TABLE IF NOT EXISTS ask_groups (
        parent_id    TEXT PRIMARY KEY,
        asker        TEXT NOT NULL,
        subject      TEXT,
        body         TEXT NOT NULL,
        opened_ts    INTEGER NOT NULL,
        target_count INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS jobs (
        id                   TEXT PRIMARY KEY,
        title                TEXT NOT NULL DEFAULT '',
        description          TEXT NOT NULL DEFAULT '',
        kind                 TEXT NOT NULL DEFAULT 'general',
        state                TEXT NOT NULL,
        state_reason         TEXT,
        phase                TEXT,
        prompt               TEXT,
        progress_note        TEXT,
        progress_events_json TEXT NOT NULL DEFAULT '[]',
        creator              TEXT NOT NULL,
        owner                TEXT,
        assignee             TEXT,
        circle               TEXT,
        correlation_id       TEXT,
        source_kind          TEXT,
        source_id            TEXT,
        scope                TEXT,
        visibility           TEXT NOT NULL DEFAULT 'circle',
        attempt_id           TEXT,
        deadline_at          INTEGER,
        expires_at           INTEGER,
        result_summary       TEXT,
        result_json          TEXT NOT NULL DEFAULT '{}',
        error_json           TEXT NOT NULL DEFAULT '{}',
        artifacts_json       TEXT NOT NULL DEFAULT '[]',
        cancel_requested     INTEGER NOT NULL DEFAULT 0,
        cancel_requested_by  TEXT,
        cancel_requested_ts  INTEGER,
        cancel_reason        TEXT,
        opened_ts            INTEGER NOT NULL,
        updated_ts           INTEGER NOT NULL,
        completed_ts         INTEGER
    )",
    "CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state)",
    "CREATE INDEX IF NOT EXISTS idx_jobs_owner_updated ON jobs(owner, updated_ts)",
    "CREATE INDEX IF NOT EXISTS idx_jobs_assignee_updated ON jobs(assignee, updated_ts)",
    "CREATE INDEX IF NOT EXISTS idx_jobs_circle_updated ON jobs(circle, updated_ts)",
    // delivery_log (P6): metadata-only transport trace. SECRET-FREE — only (ref_id,
    // ref_kind, to_peer, stage, outcome, ts); never body/subject/sig/token.
    "CREATE TABLE IF NOT EXISTS delivery_log (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        ref_id    INTEGER NOT NULL,
        ref_kind  TEXT NOT NULL,
        to_peer   TEXT NOT NULL,
        stage     TEXT NOT NULL,
        outcome   TEXT NOT NULL,
        ts        INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_delivery_log_ref ON delivery_log(ref_id, ref_kind)",
    "CREATE INDEX IF NOT EXISTS idx_delivery_log_exact ON delivery_log(ref_id, ref_kind, to_peer, stage, outcome)",
    "CREATE INDEX IF NOT EXISTS idx_delivery_log_ts ON delivery_log(ts)",
    // Token-free, exactly-one-row-per-platform bridge runtime state.
    "CREATE TABLE IF NOT EXISTS bridge_runtime (
        platform         TEXT PRIMARY KEY,
        identity         TEXT NOT NULL,
        recipient        TEXT NOT NULL,
        cursor           TEXT NOT NULL DEFAULT '',
        owner_id         TEXT NOT NULL DEFAULT '',
        owner_pid        INTEGER,
        owner_host       TEXT NOT NULL DEFAULT '',
        heartbeat_ts     INTEGER NOT NULL DEFAULT 0,
        status           TEXT NOT NULL DEFAULT 'stopped',
        last_poll_ts     INTEGER NOT NULL DEFAULT 0,
        last_success_ts  INTEGER NOT NULL DEFAULT 0,
        last_delivery_ts INTEGER NOT NULL DEFAULT 0,
        last_error_class TEXT NOT NULL DEFAULT '',
        last_error       TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS bridge_staged_events (
        platform          TEXT NOT NULL,
        external_identity TEXT NOT NULL,
        external_scope    TEXT NOT NULL,
        position          TEXT NOT NULL,
        order_key         TEXT NOT NULL,
        sender            TEXT,
        text              TEXT,
        PRIMARY KEY (platform, external_identity, external_scope, position)
    )",
    "CREATE INDEX IF NOT EXISTS idx_bridge_staged_route_order
        ON bridge_staged_events(
            platform, external_identity, external_scope, order_key, position
        )",
    // presence (v0.2 daemon): per-peer heartbeat tracking
    "CREATE TABLE IF NOT EXISTS presence (
        name         TEXT PRIMARY KEY,
        host         TEXT NOT NULL DEFAULT '',
        pid          INTEGER,
        heartbeat_ts INTEGER NOT NULL DEFAULT 0
    )",
    // WL-016: scheduled message delivery
    "CREATE TABLE IF NOT EXISTS schedules (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        kind        TEXT NOT NULL,
        cron_expr   TEXT NOT NULL,
        next_run    INTEGER NOT NULL,
        sender      TEXT NOT NULL,
        recipient   TEXT NOT NULL,
        subject     TEXT,
        body        TEXT NOT NULL,
        created_ts  INTEGER NOT NULL,
        executed_ts INTEGER,
        cancelled   INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE INDEX IF NOT EXISTS idx_schedules_next_run ON schedules(next_run)",
    "CREATE INDEX IF NOT EXISTS idx_schedules_sender    ON schedules(sender)",
    // WL-020: reviews table
    "CREATE TABLE IF NOT EXISTS reviews (\n        id                 TEXT PRIMARY KEY,\n        pr_url             TEXT NOT NULL,\n        title              TEXT NOT NULL DEFAULT '',\n        author             TEXT NOT NULL DEFAULT '',\n        repo               TEXT NOT NULL DEFAULT '',\n        state              TEXT NOT NULL DEFAULT 'open',\n        review_requested_at INTEGER,\n        reviewed_at        INTEGER,\n        reviewed_by        TEXT,\n        created_at         INTEGER NOT NULL\n    )",
    "CREATE INDEX IF NOT EXISTS idx_reviews_state ON reviews(state)",
    "CREATE INDEX IF NOT EXISTS idx_reviews_created ON reviews(created_at)",
    // WL-024: leases table
    "CREATE TABLE IF NOT EXISTS leases (\n        resource  TEXT PRIMARY KEY,\n        holder    TEXT NOT NULL,\n        acquired  INTEGER NOT NULL,\n        expires   INTEGER NOT NULL,\n        note      TEXT NOT NULL DEFAULT ''\n    )",
    "CREATE INDEX IF NOT EXISTS idx_leases_holder ON leases(holder)",
    "CREATE INDEX IF NOT EXISTS idx_leases_expires ON leases(expires)",
    // WL-028: FTS5 full-text search on messages
    "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        body, subject, sender,
        content='messages',
        content_rowid='id'
    )",
    "CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, body, subject, sender)
        VALUES (new.id, new.body, new.subject, new.sender);
    END",
    "CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, body, subject, sender)
        VALUES ('delete', old.id, old.body, old.subject, old.sender);
    END",
    // WL-033: thread summarization cache
    "CREATE TABLE IF NOT EXISTS summaries (
        root_id     INTEGER PRIMARY KEY,
        text        TEXT NOT NULL,
        model       TEXT NOT NULL DEFAULT '',
        created_ts  INTEGER NOT NULL,
        refreshed_ts INTEGER NOT NULL,
        generation  INTEGER NOT NULL DEFAULT -1
    )",
    "CREATE TABLE IF NOT EXISTS summary_state (
        singleton  INTEGER PRIMARY KEY CHECK(singleton = 1),
        generation INTEGER NOT NULL
    )",
    "INSERT OR IGNORE INTO summary_state (singleton, generation) VALUES (1, 0)",
    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_insert_v1
     AFTER INSERT ON messages BEGIN
         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
         DELETE FROM summaries;
     END",
    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_update_v1
     AFTER UPDATE ON messages BEGIN
         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
         DELETE FROM summaries;
     END",
    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_delete_v1
     AFTER DELETE ON messages BEGIN
         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
         DELETE FROM summaries;
     END",
];

/// Resolve the wall-clock bound (Duration) for a single REMOTE network call (connect
/// or a SELECT `block_on`). libsql 0.9.30 has NO client-side connect/request timeout
/// knob (`busy_timeout` is a no-op for remote — proven from the vendored crate), so
/// we wrap each remote `block_on` future in `tokio::time::timeout`. A timeout is
/// treated as just another source skip (the existing failure-isolation contract):
/// stderr + continue, never a panic, never a partial commit.
///
/// Precedence: a `per_source` value (already resolved + clamped in `config` from
/// `WEAVE_PULL_TIMEOUT_MS_<LABEL>`) wins; else the global `WEAVE_PULL_TIMEOUT_MS` (if
/// a positive integer); else [`REMOTE_TIMEOUT_MS_DEFAULT`] (owned by `config` — ONE
/// source of truth shared with the config-resolved path, drift guard). A `0`/garbage
/// global value falls back to the default; we NEVER disable the bound (an unbounded
/// remote could hang a drain).
fn remote_timeout_for(per_source: Option<u64>) -> std::time::Duration {
    let ms = per_source.unwrap_or_else(|| {
        std::env::var("WEAVE_PULL_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(REMOTE_TIMEOUT_MS_DEFAULT)
    });
    std::time::Duration::from_millis(ms)
}

pub struct LibsqlStore {
    rt: Runtime,
    conn: Connection,
    // Keep the database alive for as long as the connection is used.
    _db: Database,
    /// OWNER-ONLY-WRITES guard: `true` for a handle opened to read a FOREIGN store
    /// (local read-only or remote). Every write method traps when this is set, so
    /// weave can never mutate a store it does not own — the runtime enforcement of
    /// the cross-machine owner-only-writes invariant. The primary RW `open` sets it
    /// `false`; `open_readonly`/`open_readonly_remote` set it `true`.
    read_only: bool,
    /// The resolved per-source REMOTE-call timeout (ms) for a remote handle: `Some`
    /// only on a handle opened via [`open_readonly_remote`] with a per-source value;
    /// `None` for local/RW opens AND for a remote opened with no per-source override
    /// (in which case [`remote_timeout_for`] falls back to the global/default). Read
    /// by [`block_on_bounded`] so the bounded SELECTs honor the SAME per-source value
    /// the connect used. Inert on a local handle (`block_on_bounded` runs it
    /// unbounded).
    remote_timeout: Option<u64>,
    /// WL-035: the on-disk path of a LOCAL-file backend, or `None` for a remote
    /// (Turso) backend. `snapshot_to` needs a real local file to `VACUUM INTO`; a
    /// remote backend has none and bails. Set from `cfg.db_path()` on a local
    /// `open`, `None` for remote/read-only-remote opens.
    local_path: Option<std::path::PathBuf>,
}

/// Convert a libsql row column into our owned `Message`. Column order matches
/// the explicit projections used below: id, ts, sender, recipient, subject,
/// body, in_reply_to. Every projection now selects the 7th column so positional
/// index 6 is always present.
fn row_to_message(r: &libsql::Row) -> Result<Message> {
    Ok(Message {
        id: r.get::<i64>(0)?,
        ts: r.get::<i64>(1)?,
        sender: r.get::<String>(2)?,
        recipient: r.get::<String>(3)?,
        subject: r.get::<Option<String>>(4)?,
        body: r.get::<String>(5)?,
        in_reply_to: r.get::<Option<i64>>(6)?,
        idempotency_key: r.get::<Option<String>>(7).ok().flatten(),
        trace_id: r.get::<Option<String>>(8).ok().flatten(),
        priority: r.get::<String>(9).unwrap_or_else(|_| "normal".to_string()),
        // WL-037: positional index 10 — EVERY explicit projection feeding this
        // mapper MUST list `superseded_by` as the trailing (11th) column.
        superseded_by: r.get::<Option<i64>>(10).ok().flatten(),
        // WL-038: positional index 11 — EVERY explicit projection feeding this
        // mapper MUST list `expires_at` as the 12th column.
        expires_at: r.get::<Option<i64>>(11).ok().flatten(),
        // WL-039: positional index 12 — EVERY explicit projection feeding this
        // mapper MUST list `kind` as the trailing (13th) column.
        kind: r.get::<Option<String>>(12).ok().flatten(),
        request_priority: r.get::<Option<String>>(13).ok().flatten(),
        request_ttl: r.get::<Option<i64>>(14).ok().flatten(),
        request_supersedes: r.get::<Option<i64>>(15).ok().flatten(),
        request_dedup_idle: r
            .get::<Option<i64>>(16)
            .ok()
            .flatten()
            .map(|value| value != 0),
    })
}

const BRIDGE_RUNTIME_COLS: &str = "platform, identity, recipient, cursor, owner_id, owner_pid, \
     owner_host, heartbeat_ts, status, last_poll_ts, last_success_ts, last_delivery_ts, \
     last_error_class, last_error";

fn row_to_bridge_runtime(r: &libsql::Row) -> Result<BridgeRuntimeState> {
    let platform = BridgePlatform::from_str(&r.get::<String>(0)?).map_err(anyhow::Error::msg)?;
    let status = BridgeRuntimeStatus::from_str(&r.get::<String>(8)?).map_err(anyhow::Error::msg)?;
    Ok(BridgeRuntimeState {
        platform,
        identity: r.get::<String>(1)?,
        recipient: r.get::<String>(2)?,
        cursor: r.get::<String>(3)?,
        owner_id: r.get::<String>(4)?,
        owner_pid: r.get::<Option<i64>>(5)?,
        owner_host: r.get::<String>(6)?,
        heartbeat_ts: r.get::<i64>(7)?,
        status,
        last_poll_ts: r.get::<i64>(9)?,
        last_success_ts: r.get::<i64>(10)?,
        last_delivery_ts: r.get::<i64>(11)?,
        last_error_class: r.get::<String>(12)?,
        last_error: r.get::<String>(13)?,
    })
}

async fn update_bridge_runtime_tx_libsql(
    tx: &libsql::Transaction,
    platform: BridgePlatform,
    owner_id: &str,
    update: &BridgeRuntimeUpdate,
) -> Result<u64> {
    let (error_mode, error_class, error_message): (i64, Option<&str>, Option<&str>) =
        match &update.error {
            BridgeRuntimeErrorUpdate::Keep => (0, None, None),
            BridgeRuntimeErrorUpdate::Clear => (1, None, None),
            BridgeRuntimeErrorUpdate::Set { class, message } => {
                (2, Some(class.as_str()), Some(message.as_str()))
            }
        };
    Ok(tx
        .execute(
            "UPDATE bridge_runtime SET
                cursor = COALESCE(?3, cursor),
                status = COALESCE(?4, status),
                last_poll_ts = CASE
                    WHEN ?5 IS NULL OR ?5 <= last_poll_ts THEN last_poll_ts ELSE ?5 END,
                last_success_ts = CASE
                    WHEN ?6 IS NULL OR ?6 <= last_success_ts THEN last_success_ts ELSE ?6 END,
                last_delivery_ts = CASE
                    WHEN ?7 IS NULL OR ?7 <= last_delivery_ts THEN last_delivery_ts ELSE ?7 END,
                last_error_class = CASE ?8
                    WHEN 1 THEN '' WHEN 2 THEN ?9 ELSE last_error_class END,
                last_error = CASE ?8
                    WHEN 1 THEN '' WHEN 2 THEN ?10 ELSE last_error END,
                heartbeat_ts = ?11
             WHERE platform = ?1 AND owner_id = ?2",
            params(vec![
                platform.as_str().into(),
                owner_id.into(),
                update.cursor.as_deref().into(),
                update.status.map(BridgeRuntimeStatus::as_str).into(),
                update.last_poll_ts.into(),
                update.last_success_ts.into(),
                update.last_delivery_ts.into(),
                error_mode.into(),
                error_class.into(),
                error_message.into(),
                now().into(),
            ]),
        )
        .await?)
}

/// Convert an `outbox` row into our owned `Intent`. Column order matches the
/// explicit projections below: id, ts, to_peer, to_host, from_peer, subject,
/// body, sig.
fn row_to_intent(r: &libsql::Row) -> Result<Intent> {
    Ok(Intent {
        id: r.get::<i64>(0)?,
        ts: r.get::<i64>(1)?,
        to: r.get::<String>(2)?,
        to_host: r.get::<String>(3)?,
        from: r.get::<String>(4)?,
        subject: r.get::<Option<String>>(5)?,
        body: r.get::<String>(6)?,
        sig: r.get::<String>(7)?,
        idempotency_key: r.get::<Option<String>>(8).ok().flatten(),
        trace_id: r.get::<Option<String>>(9).ok().flatten(),
        priority: r.get::<String>(10).unwrap_or("normal".to_string()),
        // WL-038: positional index 11 — `list_outbox`/`outbox_all` project `ttl` last.
        ttl: r.get::<i64>(11).unwrap_or(0),
    })
}

/// Convert an `asks` row into our owned [`Ask`]. Column order matches the explicit
/// projections below: id, question_msg_id, answer_msg_id, asker, askee, subject,
/// state, reply_to, close_note, opened_ts, updated_ts, closed_ts. `state` is parsed
/// through [`AskState::from_str`]; an unknown value is a clean error, never a panic.
fn row_to_ask(r: &libsql::Row) -> Result<Ask> {
    let state_str = r.get::<String>(6)?;
    let state = AskState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
    let kind_str = r.get::<String>(7)?;
    let kind = AskKind::from_str(&kind_str);
    Ok(Ask {
        id: r.get::<String>(0)?,
        question_msg_id: r.get::<i64>(1)?,
        answer_msg_id: r.get::<Option<i64>>(2)?,
        asker: r.get::<String>(3)?,
        askee: r.get::<String>(4)?,
        subject: r.get::<Option<String>>(5)?,
        state,
        kind,
        options: r.get::<Option<String>>(8)?,
        reply_to: r.get::<Option<String>>(9)?,
        close_note: r.get::<Option<String>>(10)?,
        opened_ts: r.get::<i64>(11)?,
        updated_ts: r.get::<i64>(12)?,
        closed_ts: r.get::<Option<i64>>(13)?,
        // 15th projected column (P2 / WL-015). Every projection selects `parent_id`
        // last so positional index 14 is always present.
        parent_id: r.get::<Option<String>>(14)?,
    })
}

#[allow(clippy::too_many_arguments)]
async fn validate_import_ask_relations_libsql(
    tx: &libsql::Transaction,
    existing_id: Option<&str>,
    question_msg_id: i64,
    answer_msg_id: Option<i64>,
    asker: &str,
    askee: &str,
    subject: Option<&str>,
    kind: AskKind,
    options: Option<&str>,
    reply_to: Option<&str>,
    opened_ts: i64,
    parent_id: Option<&str>,
) -> Result<()> {
    let mut rows = tx
        .query(
            "SELECT COUNT(*) FROM asks
              WHERE id != COALESCE(?3, '')
                AND (question_msg_id = ?1 OR answer_msg_id = ?1
                     OR (?2 IS NOT NULL AND (question_msg_id = ?2 OR answer_msg_id = ?2)))",
            params(vec![
                question_msg_id.into(),
                answer_msg_id.into(),
                existing_id.map(str::to_string).into(),
            ]),
        )
        .await?;
    let alias_count = rows.next().await?.map_or(Ok(0), |row| row.get::<i64>(0))?;
    drop(rows);
    if alias_count != 0 {
        anyhow::bail!("imported ask message is already claimed by another ask");
    }

    let mut rows = tx
        .query(
            "SELECT sender, recipient, subject, body, in_reply_to
               FROM messages WHERE id = ?1",
            params(vec![question_msg_id.into()]),
        )
        .await?;
    let row = rows.next().await?.ok_or_else(|| {
        anyhow::anyhow!("imported ask question message #{question_msg_id} is missing")
    })?;
    let question_sender = row.get::<String>(0)?;
    let question_recipient = row.get::<String>(1)?;
    let question_subject = row.get::<Option<String>>(2)?;
    let question_body = row.get::<String>(3)?;
    let question_parent = row.get::<Option<i64>>(4)?;
    drop(rows);
    if question_sender != asker || question_recipient != askee {
        anyhow::bail!("imported ask question route does not match asker/askee");
    }
    if question_subject.as_deref() != subject {
        anyhow::bail!("imported ask subject does not match its question message");
    }
    if let Some(parent_ask_id) = reply_to {
        let mut rows = tx
            .query(
                "SELECT asker, askee, state, question_msg_id, answer_msg_id, updated_ts, closed_ts
                   FROM asks WHERE id = ?1",
                params(vec![parent_ask_id.into()]),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("imported ask reply_to '{parent_ask_id}' is missing"))?;
        let parent_asker = row.get::<String>(0)?;
        let parent_askee = row.get::<String>(1)?;
        let parent_state = row.get::<String>(2)?;
        let parent_question = row.get::<i64>(3)?;
        let parent_answer = row.get::<Option<i64>>(4)?;
        let parent_updated = row.get::<i64>(5)?;
        let parent_closed = row.get::<Option<i64>>(6)?;
        drop(rows);
        let same_pair = (parent_asker == asker && parent_askee == askee)
            || (parent_asker == askee && parent_askee == asker);
        if !same_pair || parent_state != AskState::Acked.as_str() {
            anyhow::bail!("imported chained ask has an incoherent parent");
        }
        if question_parent != Some(parent_answer.unwrap_or(parent_question)) {
            anyhow::bail!("imported chained ask question does not link to its parent thread");
        }
        if parent_updated > opened_ts || parent_closed.is_none_or(|closed| closed > opened_ts) {
            anyhow::bail!("imported chained ask timestamps precede its parent closure");
        }
    } else if question_parent.is_some() {
        anyhow::bail!("imported root ask question cannot carry in_reply_to");
    }

    if let Some(answer_id) = answer_msg_id {
        let mut rows = tx
            .query(
                "SELECT sender, recipient, subject, in_reply_to
                   FROM messages WHERE id = ?1",
                params(vec![answer_id.into()]),
            )
            .await?;
        let row = rows.next().await?.ok_or_else(|| {
            anyhow::anyhow!("imported ask answer message #{answer_id} is missing")
        })?;
        let answer_sender = row.get::<String>(0)?;
        let answer_recipient = row.get::<String>(1)?;
        let answer_subject = row.get::<Option<String>>(2)?;
        let answer_parent = row.get::<Option<i64>>(3)?;
        drop(rows);
        if answer_sender != askee
            || answer_recipient != asker
            || answer_parent != Some(question_msg_id)
            || answer_subject != reply_subject(subject)
        {
            anyhow::bail!("imported ask answer is incoherent with its question");
        }
    }

    if let Some(group_id) = parent_id {
        let mut rows = tx
            .query(
                "SELECT asker, subject, body, opened_ts, target_count
                   FROM ask_groups WHERE parent_id = ?1",
                params(vec![group_id.into()]),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("imported ask group '{group_id}' is missing"))?;
        let group_asker = row.get::<String>(0)?;
        let group_subject = row.get::<Option<String>>(1)?;
        let group_body = row.get::<String>(2)?;
        let group_opened = row.get::<i64>(3)?;
        let group_target_count = row.get::<i64>(4)?;
        drop(rows);
        if group_asker != asker
            || group_subject.as_deref() != subject
            || group_body != question_body
            || group_opened != opened_ts
            || kind != AskKind::FreeText
            || options.is_some()
            || reply_to.is_some()
            || question_parent.is_some()
        {
            anyhow::bail!("imported ask is incoherent with its ask group");
        }
        let mut rows = tx
            .query(
                "SELECT COUNT(*), SUM(CASE WHEN askee = ?2 THEN 1 ELSE 0 END)
                   FROM asks WHERE parent_id = ?1 AND id != COALESCE(?3, '')",
                params(vec![
                    group_id.into(),
                    askee.into(),
                    existing_id.map(str::to_string).into(),
                ]),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("failed to count imported ask group children"))?;
        let child_count = row.get::<i64>(0)?;
        let same_askee = row.get::<Option<i64>>(1)?.unwrap_or(0);
        drop(rows);
        if same_askee != 0 || child_count >= group_target_count {
            anyhow::bail!("imported ask group child set exceeds its target closure");
        }
    }
    Ok(())
}

/// The canonical `jobs` column projection (positional), shared by every job SELECT
/// so [`row_to_job`]'s indices stay in lock-step with the query. libsql has no
/// by-name `Row::get`, so we pin an explicit ordered list rather than `SELECT *`.
const JOB_COLS: &str = "id, title, description, kind, state, state_reason, phase, prompt, \
     progress_note, progress_events_json, creator, owner, assignee, circle, correlation_id, \
     source_kind, source_id, scope, visibility, attempt_id, deadline_at, expires_at, \
     result_summary, result_json, error_json, artifacts_json, cancel_requested, \
     cancel_requested_by, cancel_requested_ts, cancel_reason, opened_ts, updated_ts, completed_ts";

/// Convert a `jobs` row into our owned [`Job`]. Positional column order matches
/// [`JOB_COLS`]. `state` is parsed through [`JobState::from_str`] (a clean error on
/// an unknown value, never a panic); `cancel_requested` is a 0/1 INTEGER.
fn row_to_job(r: &libsql::Row) -> Result<Job> {
    let state_str = r.get::<String>(4)?;
    let state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
    Ok(Job {
        id: r.get::<String>(0)?,
        title: r.get::<String>(1)?,
        description: r.get::<String>(2)?,
        kind: r.get::<String>(3)?,
        state,
        state_reason: r.get::<Option<String>>(5)?,
        phase: r.get::<Option<String>>(6)?,
        prompt: r.get::<Option<String>>(7)?,
        progress_note: r.get::<Option<String>>(8)?,
        progress_events_json: r.get::<String>(9)?,
        creator: r.get::<String>(10)?,
        owner: r.get::<Option<String>>(11)?,
        assignee: r.get::<Option<String>>(12)?,
        circle: r.get::<Option<String>>(13)?,
        correlation_id: r.get::<Option<String>>(14)?,
        source_kind: r.get::<Option<String>>(15)?,
        source_id: r.get::<Option<String>>(16)?,
        scope: r.get::<Option<String>>(17)?,
        visibility: r.get::<String>(18)?,
        attempt_id: r.get::<Option<String>>(19)?,
        deadline_at: r.get::<Option<i64>>(20)?,
        expires_at: r.get::<Option<i64>>(21)?,
        result_summary: r.get::<Option<String>>(22)?,
        result_json: r.get::<String>(23)?,
        error_json: r.get::<String>(24)?,
        artifacts_json: r.get::<String>(25)?,
        cancel_requested: r.get::<i64>(26)? != 0,
        cancel_requested_by: r.get::<Option<String>>(27)?,
        cancel_requested_ts: r.get::<Option<i64>>(28)?,
        cancel_reason: r.get::<Option<String>>(29)?,
        opened_ts: r.get::<i64>(30)?,
        updated_ts: r.get::<i64>(31)?,
        completed_ts: r.get::<Option<i64>>(32)?,
    })
}

/// Convert a `schedules` row into our owned [`Schedule`]. Positional order
/// matches a `SELECT *` projection: id, kind, cron_expr, next_run, sender,
/// recipient, subject, body, created_ts, executed_ts, cancelled.
fn row_to_schedule(r: &libsql::Row) -> Result<Schedule> {
    let kind_str = r.get::<String>(1)?;
    let kind = ScheduleKind::from_str(&kind_str).map_err(|m| anyhow::anyhow!(m))?;
    Ok(Schedule {
        id: r.get::<i64>(0)?,
        kind,
        cron_expr: r.get::<String>(2)?,
        next_run: r.get::<i64>(3)?,
        sender: r.get::<String>(4)?,
        recipient: r.get::<String>(5)?,
        subject: r.get::<Option<String>>(6)?,
        body: r.get::<String>(7)?,
        created_ts: r.get::<i64>(8)?,
        executed_ts: r.get::<Option<i64>>(9)?,
        cancelled: r.get::<i64>(10)? != 0,
    })
}

/// Column order: name, mux, target, socket, cwd, last_seen, pid, host, repo,
/// branch, worktree_id, circle, role, turn_state, description, description_ts.
fn row_to_peer(r: &libsql::Row) -> Result<Peer> {
    Ok(Peer {
        name: r.get::<String>(0)?,
        mux: r.get::<String>(1)?,
        target: r.get::<String>(2)?,
        socket: r.get::<String>(3)?,
        cwd: r.get::<Option<String>>(4)?,
        last_seen: r.get::<i64>(5)?,
        pid: r.get::<Option<i64>>(6)?,
        host: r.get::<String>(7)?,
        repo: r.get::<String>(8)?,
        branch: r.get::<String>(9)?,
        worktree_id: r.get::<String>(10)?,
        circle: r.get::<String>(11)?,
        role: r.get::<String>(12)?,
        turn_state: r.get::<String>(13)?,
        description: r.get::<String>(14)?,
        description_ts: r.get::<i64>(15)?,
        birth_cert: r.get::<Option<String>>(16).ok().flatten(),
        contact_policy: r.get::<String>(17).unwrap_or_else(|_| "open".to_string()),
        client_session: r.get::<String>(18).unwrap_or_default(),
    })
}

impl LibsqlStore {
    pub fn open(cfg: &Config) -> Result<Self> {
        // Current-thread runtime: this store owns it and drives every async call
        // through `block_on`, keeping the public API synchronous.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for libsql backend")?;

        let path = cfg.db_path();
        // WL-035: a local-file backend (no remote URL) snapshots from this path;
        // a remote backend has no local file (`None`).
        let local_path = if cfg.libsql_url.is_none() {
            Some(path.clone())
        } else {
            None
        };

        // The open-time schema + migration writes are idempotent (CREATE TABLE IF
        // NOT EXISTS / INSERT OR IGNORE / pragma-guarded ALTER), but they RACE under
        // concurrent multi-process open: every starting `weave` process runs them
        // against the same file, and libsql's local busy_timeout is not honored, so
        // a concurrent opener can see an immediate "database is locked". Retry the
        // whole connect+migrate as a unit on that transient error — bounded, and
        // safe to re-run because every statement is idempotent.
        let mut open_attempt: u32 = 0;
        let (db, conn) = loop {
            let url = cfg.libsql_url.clone();
            let token = cfg.libsql_auth_token.clone();
            let path = path.clone();
            let attempt_res: Result<(Database, Connection)> = rt.block_on(async move {
            let is_remote = url.is_some();
            let db = if let Some(url) = url {
                Builder::new_remote(url, token.unwrap_or_default())
                    .build()
                    .await
                    .context("opening remote libsql database")?
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                Builder::new_local(&path)
                    .build()
                    .await
                    .context("opening local libsql database")?
            };
            let conn = db.connect().context("connecting to libsql database")?;
            // Mirror SqliteStore's concurrency/durability setup so a local libsql
            // file behaves identically under the multi-process contention weave is
            // built for. Without these, a local libsql file uses a 0ms busy handler
            // (immediate SQLITE_BUSY for any concurrent writer) and a rollback
            // journal (readers and writers block each other).
            conn.busy_timeout(std::time::Duration::from_secs(30))
                .context("setting libsql busy_timeout")?;
            // libsql 0.9's `busy_timeout()` API does NOT reliably install a busy
            // handler for a local-file connection: under concurrent multi-process
            // open (each process runs the idempotent open-time migrations) the
            // INSERT in migration #7 still fails *immediately* with "database is
            // locked" well inside the 30s window. Setting it at the SQL layer makes
            // the SQLite core honor it. `PRAGMA busy_timeout=N` RETURNS the applied
            // value, so it must go through `query`, not `execute`.
            conn.query("PRAGMA busy_timeout=30000", ())
                .await
                .context("setting libsql busy_timeout pragma")?;
            if !is_remote {
                // WAL + NORMAL are only meaningful for a local file; skip them on
                // the remote (hrana) path where they don't apply. `PRAGMA
                // journal_mode=WAL` RETURNS A ROW (the resulting mode), so it must
                // go through `query`, not `execute` (libsql's execute rejects any
                // statement that yields rows with "Execute returned rows").
                conn.query("PRAGMA journal_mode=WAL", ())
                    .await
                    .context("setting libsql journal_mode=WAL")?;
                conn.query("PRAGMA synchronous=NORMAL", ())
                    .await
                    .context("setting libsql synchronous=NORMAL")?;
            }
            for stmt in SCHEMA {
                conn.execute(stmt, ())
                    .await
                    .context("creating libsql schema")?;
            }
            // Migration: a DB created before threading lacks `in_reply_to`. Add it
            // idempotently (mirrors SqliteStore::migrate) so thread/reply work on
            // pre-existing stores instead of failing "no such column".
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('messages') WHERE name='in_reply_to'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute("ALTER TABLE messages ADD COLUMN in_reply_to INTEGER", ())
                    .await
                    .context("adding in_reply_to column")?;
            }
            // Migration: a DB created before kitty-socket persistence lacks
            // `peers.socket`. Add it idempotently (mirrors SqliteStore::migrate)
            // defaulting to '' for existing rows == socket unknown.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='socket'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE peers ADD COLUMN socket TEXT NOT NULL DEFAULT ''",
                    (),
                )
                .await
                .context("adding socket column")?;
            }
            // Migration: a DB created before process-liveness lacks `peers.pid`.
            // Add it idempotently (mirrors SqliteStore::migrate). Nullable;
            // defaults to NULL == PID unknown ⇒ presence falls back to the TTL
            // guess.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='pid'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute("ALTER TABLE peers ADD COLUMN pid INTEGER", ())
                    .await
                    .context("adding pid column")?;
            }
            // Migration: a DB created before process-liveness lacks `peers.host`.
            // Add it idempotently (mirrors SqliteStore::migrate) defaulting to ''
            // for existing rows == host unknown ⇒ liveness fails open / TTL-only.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='host'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE peers ADD COLUMN host TEXT NOT NULL DEFAULT ''",
                    (),
                )
                .await
                .context("adding host column")?;
            }
            // Migration: a DB created before session-scan tagging lacks the
            // `peers.repo` / `branch` / `worktree_id` columns. Add each
            // idempotently (mirrors SqliteStore::migrate) defaulting to '' for
            // existing rows == tag unknown. The DDL identifiers are constant.
            for col in ["repo", "branch", "worktree_id"] {
                let probe = format!("SELECT 1 FROM pragma_table_info('peers') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    let ddl =
                        format!("ALTER TABLE peers ADD COLUMN {col} TEXT NOT NULL DEFAULT ''");
                    conn.execute(&ddl, ())
                        .await
                        .with_context(|| format!("adding {col} column"))?;
                }
            }
            // Migration (P4): a DB created before circles + orchestrator role lacks
            // the `peers.circle` / `peers.role` columns. Add each idempotently
            // (mirrors SqliteStore::migrate). `circle` defaults to the non-empty
            // literal 'default' (legacy rows classify into the default circle with
            // no runtime coalesce); `role` defaults to 'peer' (legacy rows are plain
            // participants). The col names and the default literals are constant DDL
            // (no user data interpolated).
            for (col, default) in [("circle", "default"), ("role", "peer")] {
                let probe = format!("SELECT 1 FROM pragma_table_info('peers') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    let ddl = format!(
                        "ALTER TABLE peers ADD COLUMN {col} TEXT NOT NULL DEFAULT '{default}'"
                    );
                    conn.execute(&ddl, ())
                        .await
                        .with_context(|| format!("adding {col} column"))?;
                }
            }
            // Migration (P5): a DB created before rich presence lacks the
            // `peers.turn_state` / `description` / `description_ts` columns. Add
            // each idempotently (mirrors SqliteStore::migrate). `turn_state` and
            // `description` default to '' (== Unknown / no description for legacy
            // rows); `description_ts` defaults to 0 (no TTL anchor). The col names
            // and default literals are constant DDL (no user data interpolated).
            for col in ["turn_state", "description"] {
                let probe = format!("SELECT 1 FROM pragma_table_info('peers') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    let ddl =
                        format!("ALTER TABLE peers ADD COLUMN {col} TEXT NOT NULL DEFAULT ''");
                    conn.execute(&ddl, ())
                        .await
                        .with_context(|| format!("adding {col} column"))?;
                }
            }
            {
                let probe =
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='description_ts'";
                let mut it = conn.query(probe, ()).await?;
                if it.next().await?.is_none() {
                    conn.execute(
                        "ALTER TABLE peers ADD COLUMN description_ts INTEGER NOT NULL DEFAULT 0",
                        (),
                    )
                    .await
                    .context("adding description_ts column")?;
                }
            }
            // WL-018: birth certificate for identity takeover protection. Nullable;
            // NULL means "not yet enrolled" (backward-compat). Existing peers without
            // a cert get one minted on their next re-registration.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='birth_cert'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE peers ADD COLUMN birth_cert TEXT",
                    (),
                )
                .await
                .context("adding birth_cert column")?;
            }
            // Migration (WL-084): launcher-session key for collision-proof
            // identity. '' == unknown, matching `Peer::client_session`'s empty
            // default (mirrors the sqlite migrate). Constant DDL, idempotent.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('peers') WHERE name='client_session'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE peers ADD COLUMN client_session TEXT NOT NULL DEFAULT ''",
                    (),
                )
                .await
                .context("adding client_session column")?;
            }
            // Migration (P5): the wake-hook watermark table. Tracks the last
            // unread message id that triggered a block for each reader. Created
            // via SCHEMA above for a fresh DB; also created idempotently here for
            // a DB that predates wake. Constant DDL — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS wake_acks (
                    reader  TEXT PRIMARY KEY,
                    last_id INTEGER NOT NULL
                )",
                (),
            )
            .await
            .context("creating wake_acks table")?;
            // Migration (#7): the multi-key `identity_keys` registry. A DB created
            // before multi-key support lacks the table; create it idempotently
            // (mirrors SqliteStore::migrate) and one-time-copy the legacy single-key
            // `keys` rows into it. `INSERT OR IGNORE` keyed on the (identity,pubkey)
            // PRIMARY KEY makes the copy a clean no-op on re-run and never overwrites
            // a key already added under the new registry. The legacy `keys` table is
            // RETAINED as a deprecated shadow (no DROP) for crash-safety / old-binary
            // coexistence; new writes go ONLY to `identity_keys`. Constant DDL — no
            // user data interpolated. Standard SQLite SQL under libsql's dialect.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS identity_keys (
                    identity TEXT NOT NULL,
                    pubkey   TEXT NOT NULL,
                    added_ts INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (identity, pubkey)
                )",
                (),
            )
            .await
            .context("creating identity_keys table")?;
            conn.execute(
                "INSERT OR IGNORE INTO identity_keys (identity, pubkey, added_ts)
                    SELECT identity, pubkey, 0 FROM keys",
                (),
            )
            .await
            .context("copying legacy keys into identity_keys")?;
            // Migration (#11): the observed-revocation audit log. Created via SCHEMA
            // above for a fresh DB; also created idempotently here for a DB that
            // predates it (mirrors SqliteStore::migrate). Inert plain data in every
            // build; only the sign-gated write/read code touches it. NEVER read by the
            // verification decision, so it cannot drift from or weaken R1. Constant DDL
            // — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS revocations (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts        INTEGER NOT NULL,
                    fp        TEXT NOT NULL,
                    identity  TEXT NOT NULL DEFAULT '',
                    source    TEXT NOT NULL DEFAULT '',
                    kind      TEXT NOT NULL DEFAULT 'enforced'
                )",
                (),
            )
            .await
            .context("creating revocations table")?;
            // Migration (P1): the tracked ask/answer/ack side-table. Created via
            // SCHEMA above for a fresh DB; also created idempotently here for a DB
            // that predates it (mirrors SqliteStore::migrate). Inert plain data in
            // every build; the question/answer TEXT lives in `messages`, this row
            // holds only correlation + lifecycle. Constant DDL — no user data
            // interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS asks (
                    id              TEXT PRIMARY KEY,
                    question_msg_id INTEGER NOT NULL,
                    answer_msg_id   INTEGER,
                    asker           TEXT NOT NULL,
                    askee           TEXT NOT NULL,
                    subject         TEXT,
                    state           TEXT NOT NULL,
                    reply_to        TEXT,
                    close_note      TEXT,
                    opened_ts       INTEGER NOT NULL,
                    updated_ts      INTEGER NOT NULL,
                    closed_ts       INTEGER
                )",
                (),
            )
            .await
            .context("creating asks table")?;
            // Migration (P2): a legacy P1-era DB whose `asks` table predates ask-many
            // lacks `parent_id`. Add it idempotently (mirrors SqliteStore::migrate, the
            // `peers.pid` template) defaulting to NULL for existing rows == a standalone
            // ask, not part of a group. The `pragma_table_info` guard makes a re-run a
            // no-op; constant DDL — no user data interpolated.
            let mut it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('asks') WHERE name='parent_id'",
                    (),
                )
                .await?;
            if it.next().await?.is_none() {
                conn.execute("ALTER TABLE asks ADD COLUMN parent_id TEXT", ())
                    .await
                    .context("adding asks.parent_id column")?;
            }
            // WL-015: structured ask kinds + options.
            let mut kind_it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('asks') WHERE name='kind'",
                    (),
                )
                .await?;
            if kind_it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE asks ADD COLUMN kind TEXT NOT NULL DEFAULT 'free_text'",
                    (),
                )
                .await
                .context("adding asks.kind column")?;
            }
            let mut opt_it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('asks') WHERE name='options'",
                    (),
                )
                .await?;
            if opt_it.next().await?.is_none() {
                conn.execute("ALTER TABLE asks ADD COLUMN options TEXT", ())
                    .await
                    .context("adding asks.options column")?;
            }
            let mut request_subject_it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('asks') WHERE name='request_subject'",
                    (),
                )
                .await?;
            if request_subject_it.next().await?.is_none() {
                conn.execute("ALTER TABLE asks ADD COLUMN request_subject TEXT", ())
                    .await
                    .context("adding asks.request_subject column")?;
            }
            let mut request_subject_provided_it = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('asks') \
                     WHERE name='request_subject_provided'",
                    (),
                )
                .await?;
            if request_subject_provided_it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE asks ADD COLUMN request_subject_provided INTEGER",
                    (),
                )
                .await
                .context("adding asks.request_subject_provided column")?;
            }
            // Migration (P2): the ask-many PARENT anchor table. Created via SCHEMA above
            // for a fresh DB; also created idempotently here for a DB that predates
            // ask-many (mirrors SqliteStore::migrate). Inert plain data in every build;
            // constant DDL — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS ask_groups (
                    parent_id    TEXT PRIMARY KEY,
                    asker        TEXT NOT NULL,
                    subject      TEXT,
                    body         TEXT NOT NULL,
                    opened_ts    INTEGER NOT NULL,
                    target_count INTEGER NOT NULL
                )",
                (),
            )
            .await
            .context("creating ask_groups table")?;
            // Migration (P3): the durable poll-only job board. Created via SCHEMA
            // above for a fresh DB; also created idempotently here for a DB that
            // predates it (mirrors SqliteStore::migrate). Inert plain data in every
            // build; runner-only lease/cron/spawn columns are omitted — only board
            // metadata + the first-class `attempt_id` fencing token. Constant DDL —
            // no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS jobs (
                    id                   TEXT PRIMARY KEY,
                    title                TEXT NOT NULL DEFAULT '',
                    description          TEXT NOT NULL DEFAULT '',
                    kind                 TEXT NOT NULL DEFAULT 'general',
                    state                TEXT NOT NULL,
                    state_reason         TEXT,
                    phase                TEXT,
                    prompt               TEXT,
                    progress_note        TEXT,
                    progress_events_json TEXT NOT NULL DEFAULT '[]',
                    creator              TEXT NOT NULL,
                    owner                TEXT,
                    assignee             TEXT,
                    circle               TEXT,
                    correlation_id       TEXT,
                    source_kind          TEXT,
                    source_id            TEXT,
                    scope                TEXT,
                    visibility           TEXT NOT NULL DEFAULT 'circle',
                    attempt_id           TEXT,
                    deadline_at          INTEGER,
                    expires_at           INTEGER,
                    result_summary       TEXT,
                    result_json          TEXT NOT NULL DEFAULT '{}',
                    error_json           TEXT NOT NULL DEFAULT '{}',
                    artifacts_json       TEXT NOT NULL DEFAULT '[]',
                    cancel_requested     INTEGER NOT NULL DEFAULT 0,
                    cancel_requested_by  TEXT,
                    cancel_requested_ts  INTEGER,
                    cancel_reason        TEXT,
                    opened_ts            INTEGER NOT NULL,
                    updated_ts           INTEGER NOT NULL,
                    completed_ts         INTEGER
                )",
                (),
            )
            .await
            .context("creating jobs table")?;
            for idx in [
                "CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state)",
                "CREATE INDEX IF NOT EXISTS idx_jobs_owner_updated ON jobs(owner, updated_ts)",
                "CREATE INDEX IF NOT EXISTS idx_jobs_assignee_updated ON jobs(assignee, updated_ts)",
                "CREATE INDEX IF NOT EXISTS idx_jobs_circle_updated ON jobs(circle, updated_ts)",
            ] {
                conn.execute(idx, ()).await.context("creating jobs index")?;
            }
            // Migration (P6): the metadata-only delivery trace. Created via SCHEMA
            // above for a fresh DB; also created idempotently here for a DB that
            // predates it (mirrors SqliteStore::migrate). SECRET-FREE — only (ref_id,
            // ref_kind, to_peer, stage, outcome, ts); never body/subject/sig/token.
            // Constant DDL — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS delivery_log (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    ref_id    INTEGER NOT NULL,
                    ref_kind  TEXT NOT NULL,
                    to_peer   TEXT NOT NULL,
                    stage     TEXT NOT NULL,
                    outcome   TEXT NOT NULL,
                    ts        INTEGER NOT NULL
                )",
                (),
            )
            .await
            .context("creating delivery_log table")?;
            for idx in [
                "CREATE INDEX IF NOT EXISTS idx_delivery_log_ref ON delivery_log(ref_id, ref_kind)",
                "CREATE INDEX IF NOT EXISTS idx_delivery_log_exact ON delivery_log(ref_id, ref_kind, to_peer, stage, outcome)",
                "CREATE INDEX IF NOT EXISTS idx_delivery_log_ts ON delivery_log(ts)",
            ] {
                conn.execute(idx, ())
                    .await
                    .context("creating delivery_log index")?;
            }
            // Migration-safe, token-free bridge ownership/cursor state. The
            // platform primary key is the exact singleton boundary; no provider
            // credential is stored in this table.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS bridge_runtime (
                    platform         TEXT PRIMARY KEY,
                    identity         TEXT NOT NULL,
                    recipient        TEXT NOT NULL,
                    cursor           TEXT NOT NULL DEFAULT '',
                    owner_id         TEXT NOT NULL DEFAULT '',
                    owner_pid        INTEGER,
                    owner_host       TEXT NOT NULL DEFAULT '',
                    heartbeat_ts     INTEGER NOT NULL DEFAULT 0,
                    status           TEXT NOT NULL DEFAULT 'stopped',
                    last_poll_ts     INTEGER NOT NULL DEFAULT 0,
                    last_success_ts  INTEGER NOT NULL DEFAULT 0,
                    last_delivery_ts INTEGER NOT NULL DEFAULT 0,
                    last_error_class TEXT NOT NULL DEFAULT '',
                    last_error       TEXT NOT NULL DEFAULT ''
                )",
                (),
            )
            .await
            .context("creating bridge_runtime table")?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS bridge_staged_events (
                    platform          TEXT NOT NULL,
                    external_identity TEXT NOT NULL,
                    external_scope    TEXT NOT NULL,
                    position          TEXT NOT NULL,
                    order_key         TEXT NOT NULL,
                    sender            TEXT,
                    text              TEXT,
                    PRIMARY KEY (platform, external_identity, external_scope, position)
                )",
                (),
            )
            .await
            .context("creating bridge_staged_events table")?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_bridge_staged_route_order
                    ON bridge_staged_events(
                        platform, external_identity, external_scope, order_key, position
                    )",
                (),
            )
            .await
            .context("creating bridge_staged_events index")?;
            // Migration (v0.2): presence table for daemon heartbeats. Created via
            // SCHEMA above for a fresh DB; also created idempotently here for a DB
            // that predates it. Constant DDL — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS presence (
                    name         TEXT PRIMARY KEY,
                    host         TEXT NOT NULL DEFAULT '',
                    pid          INTEGER,
                    heartbeat_ts INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await
            .context("creating presence table")?;
            // WL-016: schedules table for legacy DBs. Created via SCHEMA for fresh DBs;
            // also created idempotently here. Constant DDL — no user data interpolated.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS schedules (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind        TEXT NOT NULL,
                    cron_expr   TEXT NOT NULL,
                    next_run    INTEGER NOT NULL,
                    sender      TEXT NOT NULL,
                    recipient   TEXT NOT NULL,
                    subject     TEXT,
                    body        TEXT NOT NULL,
                    created_ts  INTEGER NOT NULL,
                    executed_ts INTEGER,
                    cancelled   INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await
            .context("creating schedules table")?;
            for idx in [
                "CREATE INDEX IF NOT EXISTS idx_schedules_next_run ON schedules(next_run)",
                "CREATE INDEX IF NOT EXISTS idx_schedules_sender    ON schedules(sender)",
            ] {
                conn.execute(idx, ())
                    .await
                    .context("creating schedules index")?;
            }
            // WL-020: reviews table
            conn.execute(
                "CREATE TABLE IF NOT EXISTS reviews (\n                    id                 TEXT PRIMARY KEY,\n                    pr_url             TEXT NOT NULL,\n                    title              TEXT NOT NULL DEFAULT '',\n                    author             TEXT NOT NULL DEFAULT '',\n                    repo               TEXT NOT NULL DEFAULT '',\n                    state              TEXT NOT NULL DEFAULT 'open',\n                    review_requested_at INTEGER,\n                    reviewed_at        INTEGER,\n                    reviewed_by        TEXT,\n                    created_at         INTEGER NOT NULL\n                )",
                (),
            )
            .await
            .context("creating reviews table")?;
            for idx in [
                "CREATE INDEX IF NOT EXISTS idx_reviews_state ON reviews(state)",
                "CREATE INDEX IF NOT EXISTS idx_reviews_created ON reviews(created_at)",
            ] {
                conn.execute(idx, ())
                    .await
                    .context("creating reviews index")?;
            }
            // WL-024: leases table
            conn.execute(
                "CREATE TABLE IF NOT EXISTS leases (\n                    resource  TEXT PRIMARY KEY,\n                    holder    TEXT NOT NULL,\n                    acquired  INTEGER NOT NULL,\n                    expires   INTEGER NOT NULL,\n                    note      TEXT NOT NULL DEFAULT ''\n                )",
                (),
            )
            .await
            .context("creating leases table")?;
            for idx in [
                "CREATE INDEX IF NOT EXISTS idx_leases_holder ON leases(holder)",
                "CREATE INDEX IF NOT EXISTS idx_leases_expires ON leases(expires)",
            ] {
                conn.execute(idx, ())
                    .await
                    .context("creating leases index")?;
            }
            // WL-026: idempotency keys and trace IDs on messages and outbox.
            // SQLite `ALTER TABLE ADD COLUMN` rejects inline UNIQUE on non-empty tables,
            // so we add the column plain then create the unique index separately.
            for (table, col, ddl) in [
                ("messages", "idempotency_key", "ALTER TABLE messages ADD COLUMN idempotency_key TEXT"),
                ("messages", "trace_id", "ALTER TABLE messages ADD COLUMN trace_id TEXT"),
                ("outbox", "idempotency_key", "ALTER TABLE outbox ADD COLUMN idempotency_key TEXT"),
                ("outbox", "trace_id", "ALTER TABLE outbox ADD COLUMN trace_id TEXT"),
            ] {
                let probe = format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    conn.execute(ddl, ()).await.with_context(|| format!("adding {table}.{col} column"))?;
                }
            }
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_idempotency_key ON messages(idempotency_key)",
                (),
            )
            .await
            .context("creating idempotency_key unique index")?;
            // This backfill deliberately runs after the WL-026 column migration
            // above. Genuine pre-WL-026 stores already have `messages` but not
            // `messages.idempotency_key`; referencing it while migrating `asks`
            // would make those stores fail to open before the additive column step.
            conn.execute(
                "UPDATE asks
                    SET request_subject = subject,
                        request_subject_provided = 1
                  WHERE request_subject_provided IS NULL
                    AND question_msg_id IN (
                        SELECT id FROM messages WHERE idempotency_key IS NOT NULL
                    )",
                (),
            )
            .await
            .context("backfilling keyed ask request subjects")?;
            // V2 signatures bind idempotency_key. Fail closed before the legacy
            // collision cleanup can null a signed row's key and make its wire
            // semantics unverifiable.
            let mut signed_collisions = conn
                .query(
                    "SELECT o.id
                       FROM outbox o
                      WHERE substr(o.sig, 1, 3) = 'v2:'
                        AND (
                            o.idempotency_key IS NULL
                            OR (
                                EXISTS (
                                    SELECT 1 FROM messages m
                                     WHERE m.idempotency_key = o.idempotency_key
                                )
                                OR EXISTS (
                                    SELECT 1 FROM outbox earlier
                                     WHERE earlier.idempotency_key = o.idempotency_key
                                       AND earlier.id < o.id
                                )
                            )
                        )
                      ORDER BY o.id ASC LIMIT 1",
                    (),
                )
                .await
                .context("checking signed v2 idempotency collisions")?;
            if let Some(row) = signed_collisions.next().await? {
                let id = row.get::<i64>(0)?;
                anyhow::bail!(
                    "refusing idempotency migration: signed v2 outbox row #{id} has a missing or \
                     colliding key; its signature binds that key, so back up and repair the row offline \
                     without editing the key independently of its signature"
                );
            }
            drop(signed_collisions);
            conn.execute(
                "UPDATE outbox SET idempotency_key = NULL
                 WHERE idempotency_key IS NOT NULL
                   AND idempotency_key IN (
                       SELECT idempotency_key FROM messages
                       WHERE idempotency_key IS NOT NULL
                   )",
                (),
            )
            .await
            .context("normalizing cross-route idempotency key collisions")?;
            conn.execute(
                "UPDATE outbox SET idempotency_key = NULL
                 WHERE idempotency_key IS NOT NULL
                   AND id NOT IN (
                       SELECT MIN(id) FROM outbox
                       WHERE idempotency_key IS NOT NULL
                       GROUP BY idempotency_key
                   )",
                (),
            )
            .await
            .context("normalizing legacy duplicate outbox idempotency keys")?;
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_outbox_idempotency_key
                 ON outbox(idempotency_key)",
                (),
            )
            .await
            .context("creating outbox idempotency_key unique index")?;
            // WL-028: FTS5 full-text search on messages.
            conn.execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                    body, subject, sender,
                    content='messages',
                    content_rowid='id'
                )",
                (),
            )
            .await
            .context("creating messages_fts virtual table")?;
            conn.execute(
                "CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, body, subject, sender)
                    VALUES (new.id, new.body, new.subject, new.sender);
                END",
                (),
            )
            .await
            .context("creating messages_fts insert trigger")?;
            conn.execute(
                "CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, body, subject, sender)
                    VALUES ('delete', old.id, old.body, old.subject, old.sender);
                END",
                (),
            )
            .await
            .context("creating messages_fts delete trigger")?;
            // WL-031: message priority levels.
            for (table, col, ddl) in [
                ("messages", "priority", "ALTER TABLE messages ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'"),
                ("outbox", "priority", "ALTER TABLE outbox ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'"),
                // WL-037: nullable (NULL == not superseded), no DEFAULT.
                ("messages", "superseded_by", "ALTER TABLE messages ADD COLUMN superseded_by INTEGER"),
                // WL-038: ephemeral absolute deadline. Nullable (NULL == permanent),
                // no DEFAULT; outbox carries the *relative* ttl (re-stamped on commit).
                ("messages", "expires_at", "ALTER TABLE messages ADD COLUMN expires_at INTEGER"),
                ("outbox", "ttl", "ALTER TABLE outbox ADD COLUMN ttl INTEGER NOT NULL DEFAULT 0"),
                // WL-039: idle-notification marker. Nullable (NULL == ordinary
                // message), no DEFAULT; set to 'idle' only on the notify dedup path.
                ("messages", "kind", "ALTER TABLE messages ADD COLUMN kind TEXT"),
                ("messages", "request_supersedes", "ALTER TABLE messages ADD COLUMN request_supersedes INTEGER"),
                ("messages", "request_priority", "ALTER TABLE messages ADD COLUMN request_priority TEXT"),
                ("messages", "request_ttl", "ALTER TABLE messages ADD COLUMN request_ttl INTEGER"),
                ("messages", "request_dedup_idle", "ALTER TABLE messages ADD COLUMN request_dedup_idle INTEGER"),
            ] {
                let probe = format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    conn.execute(ddl, ()).await.with_context(|| format!("adding {table}.{col} column"))?;
                }
            }
            for (sql, context) in [
                (
                    "UPDATE messages AS old
                        SET superseded_by = NULL
                      WHERE superseded_by IS NOT NULL
                        AND NOT EXISTS (
                            SELECT 1 FROM messages AS new
                             WHERE new.id = old.superseded_by
                               AND new.id > old.id
                               AND new.sender = old.sender
                               AND new.recipient = old.recipient
                        )",
                    "normalizing legacy successor routes",
                ),
                (
                    "UPDATE messages SET request_priority = priority
                     WHERE idempotency_key IS NOT NULL AND request_priority IS NULL",
                    "backfilling configured-send priority metadata",
                ),
                (
                    "UPDATE messages SET request_ttl = CASE
                         WHEN expires_at IS NULL THEN 0
                         ELSE MAX(expires_at - ts, 0)
                     END
                     WHERE idempotency_key IS NOT NULL AND request_ttl IS NULL",
                    "backfilling configured-send ttl metadata",
                ),
                (
                    "UPDATE messages SET request_supersedes = CASE
                         WHEN kind = 'idle' THEN 0
                         ELSE COALESCE((
                             SELECT MIN(old.id) FROM messages old
                             WHERE old.superseded_by = messages.id
                         ), 0)
                     END
                     WHERE idempotency_key IS NOT NULL AND request_supersedes IS NULL",
                    "backfilling configured-send predecessor metadata",
                ),
                (
                    "UPDATE messages
                     SET request_dedup_idle = CASE WHEN kind = 'idle' THEN 1 ELSE 0 END
                     WHERE idempotency_key IS NOT NULL AND request_dedup_idle IS NULL",
                    "backfilling configured-send idle metadata",
                ),
            ] {
                conn.execute(sql, ()).await.context(context)?;
            }
            // WL-032: per-peer contact policies.
            let probe = "SELECT 1 FROM pragma_table_info('peers') WHERE name='contact_policy'";
            let mut it = conn.query(probe, ()).await?;
            if it.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE peers ADD COLUMN contact_policy TEXT NOT NULL DEFAULT 'open'",
                    (),
                )
                .await
                .context("adding peers.contact_policy column")?;
            }
            // WL-033: thread summarization cache.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS summaries (
                    root_id     INTEGER PRIMARY KEY,
                    text        TEXT NOT NULL,
                    model       TEXT NOT NULL DEFAULT '',
                    created_ts  INTEGER NOT NULL,
                    refreshed_ts INTEGER NOT NULL,
                    generation  INTEGER NOT NULL DEFAULT -1
                )",
                (),
            )
            .await
            .context("creating summaries table")?;
            let mut summary_columns = conn
                .query(
                    "SELECT 1 FROM pragma_table_info('summaries') WHERE name='generation'",
                    (),
                )
                .await?;
            if summary_columns.next().await?.is_none() {
                conn.execute(
                    "ALTER TABLE summaries
                     ADD COLUMN generation INTEGER NOT NULL DEFAULT -1",
                    (),
                )
                .await
                .context("adding summaries.generation column")?;
            }
            drop(summary_columns);
            conn.execute(
                "CREATE TABLE IF NOT EXISTS summary_state (
                    singleton  INTEGER PRIMARY KEY CHECK(singleton = 1),
                    generation INTEGER NOT NULL
                )",
                (),
            )
            .await
            .context("creating summary generation state")?;
            conn.execute(
                "INSERT OR IGNORE INTO summary_state (singleton, generation) VALUES (1, 0)",
                (),
            )
            .await
            .context("initializing summary generation state")?;
            for (ddl, context) in [
                (
                    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_insert_v1
                     AFTER INSERT ON messages BEGIN
                         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
                         DELETE FROM summaries;
                     END",
                    "creating summary message-insert invalidation trigger",
                ),
                (
                    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_update_v1
                     AFTER UPDATE ON messages BEGIN
                         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
                         DELETE FROM summaries;
                     END",
                    "creating summary message-update invalidation trigger",
                ),
                (
                    "CREATE TRIGGER IF NOT EXISTS summaries_generation_message_delete_v1
                     AFTER DELETE ON messages BEGIN
                         UPDATE summary_state SET generation = generation + 1 WHERE singleton = 1;
                         DELETE FROM summaries;
                     END",
                    "creating summary message-delete invalidation trigger",
                ),
            ] {
                conn.execute(ddl, ()).await.context(context)?;
            }
            // A local libsql DB file must not be world/group readable (message
            // bodies). Mirror SqliteStore's 0600 hardening; no-op on the remote path.
            #[cfg(unix)]
            if !is_remote {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mut perm = meta.permissions();
                    perm.set_mode(0o600);
                    let _ = std::fs::set_permissions(&path, perm);
                }
            }
            Ok::<_, anyhow::Error>((db, conn))
            });
            match attempt_res {
                Err(e) if is_db_locked(&e) && open_attempt < 200 => {
                    open_attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(
                        (open_attempt as u64).min(20),
                    ));
                }
                other => break other?,
            }
        };

        Ok(Self {
            rt,
            conn,
            _db: db,
            read_only: false,
            remote_timeout: None,
            local_path,
        })
    }

    /// OWNER-ONLY-WRITES guard: bail loudly if a write is attempted on a read-only
    /// (foreign / remote) handle. Called at the top of every write method. This is a
    /// runtime trap converting "we promise not to write a foreign store" into an
    /// enforced invariant that returns an `Err` (NEVER a panic) in every build, so a
    /// caller mis-wiring degrades to the same failure-isolation skip as any other
    /// foreign-store error rather than aborting the process. A read-only handle is
    /// never given to a write-path in correct code, so this is a defense-in-depth
    /// backstop; the existing read-only-handle tests assert it returns `Err` and
    /// performs no write.
    fn guard_writable(&self) -> Result<()> {
        if self.read_only {
            anyhow::bail!("BUG: write attempted on a read-only foreign store");
        }
        Ok(())
    }

    /// Open an EXISTING local-file store **read-only** for Tier-1 federation.
    ///
    /// O-F3 RESOLVED: libsql 0.9 *does* expose a structural read-only open —
    /// `Builder::new_local(path).flags(OpenFlags::SQLITE_OPEN_READ_ONLY)` opens the
    /// underlying SQLite core with `SQLITE_OPEN_READONLY` (and WITHOUT
    /// `SQLITE_OPEN_CREATE`), so the engine itself rejects any write and a missing
    /// file errors rather than being created. The read-only guarantee is therefore
    /// structural on the libsql backend too, exactly like the sqlite backend — it
    /// is NOT gated off. We deliberately run NO `SCHEMA`, NO migration, and NO
    /// permission hardening: a foreign store we do not own is read exactly as-is.
    /// Remote (Turso) stores are out of scope for federation; this opens only a
    /// local file path.
    pub fn open_readonly(path: &std::path::Path) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for read-only libsql store")?;
        let path = path.to_path_buf();
        let (db, conn) = rt.block_on(async move {
            let db = Builder::new_local(&path)
                .flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
                .build()
                .await
                .context("opening read-only libsql database")?;
            let conn = db
                .connect()
                .context("connecting to read-only libsql database")?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .context("setting read-only libsql busy_timeout")?;
            Ok::<_, anyhow::Error>((db, conn))
        })?;
        Ok(Self {
            rt,
            conn,
            _db: db,
            read_only: true,
            remote_timeout: None,
            local_path: None,
        })
    }

    /// Tier-2 v2: open a REMOTE libSQL/Turso store **read-only** for cross-store
    /// federation/pull. weave NEVER writes a remote store — the owner-only-writes
    /// invariant holds cross-machine:
    ///
    /// - `read_only` is set so every write method traps ([`guard_writable`]);
    /// - NO `SCHEMA`, NO migration, NO permission hardening is run (a remote store we
    ///   do not own is read exactly as-is). Only SELECTs touch the handle;
    /// - libsql 0.9.30 has NO client-side read-only mode for a pure remote connection
    ///   (read-only is a server-side Turso token-scope property — a server-enforced
    ///   read-only token is the recommended deployment contract; see docs). Our
    ///   client-side enforcement is the read-only flag + the SELECT-only code path;
    /// - the connect is wrapped in [`remote_timeout_for`] so an unreachable remote
    ///   cannot hang the caller — a timeout surfaces as an `Err`, which the
    ///   pull/federation free fns treat as a per-source skip (stderr + continue). The
    ///   resolved per-source `timeout_ms` (from `config`) bounds BOTH the connect and
    ///   the later SELECTs (stored on the handle for [`block_on_bounded`]); `None`
    ///   falls back to the global/default exactly as before.
    ///
    /// `Builder::new_remote` creates NO local file (the `DbType::Remote` carries no
    /// path), so this leaves no local artifact. The `token` is a SECRET: it reaches
    /// only the libsql client, never a shell/argv/SQL/log. `timeout_ms` is a plain
    /// integer (not a secret).
    pub fn open_readonly_remote(
        url: &str,
        token: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for read-only remote libsql store")?;
        let url = url.to_string();
        let token = token.unwrap_or_default().to_string();
        let dur = remote_timeout_for(timeout_ms);
        let (db, conn) = rt.block_on(async move {
            let build = Builder::new_remote(url, token).build();
            // Bound the connect: an unreachable remote must not hang the drain.
            let db = tokio::time::timeout(dur, build)
                .await
                .map_err(|_| anyhow::anyhow!("remote connect timed out after {dur:?}"))?
                .context("opening read-only remote libsql database")?;
            // `connect()` is synchronous (no network) — sets up the client handle.
            let conn = db
                .connect()
                .context("connecting to read-only remote libsql database")?;
            Ok::<_, anyhow::Error>((db, conn))
        })?;
        Ok(Self {
            rt,
            conn,
            _db: db,
            read_only: true,
            remote_timeout: timeout_ms,
            local_path: None,
        })
    }

    /// Run a SELECT-bearing future on the runtime, bounded by
    /// [`remote_timeout_for`] (honoring this handle's resolved per-source
    /// `remote_timeout`) when the handle is remote/read-only. Used by the read-only
    /// foreign-handle SELECTs ([`list_peers`]/[`sessions`]/[`list_outbox`]) so a
    /// slow/unreachable remote surfaces as an `Err` (a source skip) rather than
    /// hanging. A local handle runs unbounded (its `busy_timeout` already bounds local
    /// lock contention).
    fn block_on_bounded<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        if self.read_only {
            let dur = remote_timeout_for(self.remote_timeout);
            self.rt.block_on(async move {
                tokio::time::timeout(dur, fut)
                    .await
                    .map_err(|_| anyhow::anyhow!("remote query timed out after {dur:?}"))?
            })
        } else {
            self.rt.block_on(fut)
        }
    }
}

/// Diagnostic mirror of `store::federation_status`: of the configured `extra`
/// read-only stores, how many open + list cleanly vs. are skipped this run. Used
/// by `doctor` to surface federation health. Read-only + best-effort.
pub fn federation_status(extra: &[StoreSource]) -> (usize, usize) {
    let mut ok = 0usize;
    let mut skipped = 0usize;
    for src in extra {
        match open_source_readonly(src).and_then(|s| s.list_peers()) {
            Ok(_) => ok += 1,
            Err(_) => skipped += 1,
        }
    }
    (ok, skipped)
}

/// Open a [`StoreSource`] **read-only** (the ONLY foreign touch): a `Local` path via
/// [`LibsqlStore::open_readonly`], a `Remote` URL via
/// [`LibsqlStore::open_readonly_remote`]. Both set the `read_only` write-guard flag;
/// the remote open is timeout-bounded. weave never opens a foreign source writable.
fn open_source_readonly(src: &StoreSource) -> Result<LibsqlStore> {
    match src {
        StoreSource::Local(path) => LibsqlStore::open_readonly(path),
        StoreSource::Remote {
            url,
            token,
            timeout_ms,
        } => LibsqlStore::open_readonly_remote(url, token.as_deref(), *timeout_ms),
    }
}

/// Short display label for a federated source's origin tag: a local store's basename
/// or a remote URL's redacted scheme+host (NEVER the token). Used to tag `Foreign`
/// rows and in skip diagnostics.
fn source_label(src: &StoreSource) -> String {
    match src {
        StoreSource::Local(path) => store_label(path),
        StoreSource::Remote { url, .. } => remote_scheme_host(url),
    }
}

/// Aggregate the local store's peers with those of each configured read-only
/// extra store (Tier-1 federation), origin-tagged and deduped on `(name, host)`.
/// Mirrors `store::federated_peers`: each foreign store is opened **read-only**
/// via [`LibsqlStore::open_readonly`] (structurally incapable of writing it) and
/// listed via the existing `list_peers`. **Failure isolation:** an unreadable /
/// locked / missing / non-weave extra store is logged to **stderr** and skipped —
/// it never breaks the local listing. With `extra` empty this is exactly
/// `local.list_peers()` tagged `Local`.
pub fn federated_peers(local: &dyn Store, extra: &[StoreSource]) -> Result<Vec<PeerView>> {
    let mut views: Vec<PeerView> = local
        .list_peers()?
        .into_iter()
        .map(|peer| PeerView {
            peer,
            origin: Origin::Local,
        })
        .collect();
    for src in extra {
        let label = source_label(src);
        match open_source_readonly(src).and_then(|s| s.list_peers()) {
            Ok(peers) => {
                for peer in peers {
                    views.push(PeerView {
                        peer,
                        origin: Origin::Foreign(label.clone()),
                    });
                }
            }
            Err(e) => {
                eprintln!("[weave] skipping federated store '{label}': {e}");
            }
        }
    }
    Ok(merge_peer_views(views))
}

/// Aggregate the local store's sessions with those of each configured read-only
/// extra store (Tier-1 federation). Mirrors `store::federated_sessions`: merges
/// by name keeping `max(last_activity)` and never summing unread, with the same
/// read-only open + per-store failure isolation as [`federated_peers`].
pub fn federated_sessions(local: &dyn Store, extra: &[StoreSource]) -> Result<Vec<SessionView>> {
    let mut views: Vec<SessionView> = local
        .sessions()?
        .into_iter()
        .map(|(name, unread, last_activity)| SessionView {
            name,
            unread,
            last_activity,
            origin: Origin::Local,
        })
        .collect();
    for src in extra {
        let label = source_label(src);
        match open_source_readonly(src).and_then(|s| s.sessions()) {
            Ok(sessions) => {
                for (name, unread, last_activity) in sessions {
                    views.push(SessionView {
                        name,
                        unread,
                        last_activity,
                        origin: Origin::Foreign(label.clone()),
                    });
                }
            }
            Err(e) => {
                eprintln!("[weave] skipping federated store '{label}': {e}");
            }
        }
    }
    Ok(merge_session_views(views))
}

/// Tier-2 cross-store delivery (receiver side) — libsql mirror of
/// `store::pull_from_store`. For each `allow`-listed source, open it **read-only**
/// via [`LibsqlStore::open_readonly`] (the SQLite core rejects any write), read the
/// intents addressed to `me` since this store's per-source cursor, and commit each
/// into the LOCAL store via the shared `commit_pulled` (which uses the normal
/// `Store::send` + advances the cursor). EVERY write is to `local`; the source is
/// never written, migrated, or created — the owner-only-writes guarantee is
/// structural. Unreadable/locked/missing/no-`outbox` sources are skipped (stderr),
/// never fatal; per-source commits are bounded by [`MAX_PULL_PER_DRAIN`].
///
/// `policy` (`VerifyPolicy`, 2d) is forwarded to `commit_pulled`: it carries the
/// trust set, revocation list, and tri-state strict override that decide whether an
/// unsigned/unverifiable intent is committed or dropped. A revoked key's signed
/// message and a forged signature are always rejected. Inert without the `sign`
/// feature. Signature parity with `store::pull_from_store` (same shared
/// `commit_pulled`).
pub fn pull_from_store(
    local: &dyn Store,
    me: &str,
    allow: &[StoreSource],
    policy: &VerifyPolicy,
) -> Result<Pulled> {
    let mut out = Pulled::default();
    for src in allow {
        // Per-source pull_cursor key: a Local path canonicalizes (so `./a.db` and
        // its absolute form share one cursor); a Remote URL is keyed by the exact
        // (trailing-slash-normalized) URL string — a URL is never canonicalized.
        let source = source_cursor_key(src);
        let label = source_label(src);
        let foreign = match open_source_readonly(src) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[weave] skipping pull source '{label}': {e}");
                out.sources_skipped += 1;
                continue;
            }
        };
        let cursor_key =
            crate::store::pull_cursor_scope_key(&source, me, &crate::config::this_host());
        let scoped_cursor = local.pull_cursor_get(&cursor_key)?;
        let legacy = local.pull_cursor_get(&source)?;
        let legacy_keyless_cutoff = (legacy > scoped_cursor).then_some(legacy);
        let since = scoped_cursor;
        let intents = match foreign.list_outbox(me, since, MAX_PULL_PER_DRAIN) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[weave] skipping pull source '{label}': {e}");
                out.sources_skipped += 1;
                continue;
            }
        };
        let outcome = crate::store::commit_pulled_outcome(
            local,
            me,
            &source,
            policy,
            intents,
            legacy_keyless_cutoff,
        )?;
        out.committed += outcome.committed;
        if outcome.legacy_keyless_skipped > 0 {
            eprintln!(
                "[weave] pull route '{me}' skipped {} ambiguous legacy keyless intent(s) \
                 from '{label}' while preserving at-most-once delivery; keyed legacy rows \
                 were reconciled automatically",
                outcome.legacy_keyless_skipped
            );
        }
        if outcome.committed > 0 {
            out.committed_sources.push(src.clone());
        }
    }
    Ok(out)
}

/// The `pull_cursor.source` key for a [`StoreSource`]: a Local path canonicalized
/// (matching the federation dedup discipline), or a Remote URL trailing-slash-
/// normalized (NEVER `std::fs::canonicalize`'d). Stable across runs for a stable
/// configured URL, so the idempotent high-water cursor holds cross-machine.
fn source_cursor_key(src: &StoreSource) -> String {
    match src {
        StoreSource::Local(path) => canonical_source(path),
        StoreSource::Remote { url, .. } => url.strip_suffix('/').unwrap_or(url).to_string(),
    }
}

/// Build a positional parameter vector for a libsql query. `Value` already has
/// `From` impls for the primitives we use (i64, &str, String, Option<&str>).
fn params(values: Vec<Value>) -> Vec<Value> {
    values
}

impl Store for LibsqlStore {
    fn backend(&self) -> &'static str {
        "libsql"
    }

    fn send_configured_idempotent_mode(
        &self,
        sender: &str,
        recipient: &str,
        subject: Option<&str>,
        body: &str,
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
        priority: Option<&str>,
        effective_priority: Option<&str>,
        supersedes: Option<i64>,
        ttl: i64,
        dedup_idle: bool,
        record_request_tuple: bool,
        apply_dedup_effects: bool,
    ) -> Result<(i64, bool)> {
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_ident("recipient", recipient)?;
        crate::store::check_subject(subject)?;
        check_body(body)?;
        if let Some(key) = idempotency_key {
            if !crate::model::idempotency_key_valid(key) {
                anyhow::bail!("idempotency_key is invalid or too long.");
            }
        }
        if let Some(id) = trace_id {
            if !crate::model::trace_id_valid(id) {
                anyhow::bail!("trace_id is invalid or too long.");
            }
        }
        if ttl != 0 && !crate::model::ttl_valid(ttl) {
            anyhow::bail!(
                "ttl must be 0 or between 1 and {} seconds.",
                crate::model::MAX_MSG_TTL_SECS
            );
        }
        if supersedes.is_some_and(|id| id <= 0) {
            anyhow::bail!("supersedes must be a positive message id.");
        }
        if dedup_idle && supersedes.is_some() {
            anyhow::bail!("dedup_idle and supersedes cannot be combined.");
        }
        if !record_request_tuple
            && (priority.is_some()
                || effective_priority.is_none()
                || supersedes.is_some()
                || ttl != 0
                || dedup_idle)
        {
            anyhow::bail!("plain session restore carries only effective priority.");
        }
        let request_priority = crate::model::MessagePriority::parse(priority.unwrap_or("normal"));
        let request_priority = request_priority.as_str().to_string();
        let effective_priority = crate::model::MessagePriority::parse(
            effective_priority.unwrap_or(request_priority.as_str()),
        );
        let effective_priority = effective_priority.as_str().to_string();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            if let Some(key) = idempotency_key {
                let mut outbox = tx
                    .query(
                        "SELECT id FROM outbox WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if outbox.next().await?.is_some() {
                    anyhow::bail!(
                        "idempotency key is already associated with a cross-store intent."
                    );
                }
                drop(outbox);
                let mut it = tx
                    .query(
                        "SELECT m.id, m.sender, m.recipient, m.subject, m.body, m.in_reply_to,
                                EXISTS(SELECT 1 FROM asks a
                                       WHERE a.question_msg_id = m.id OR a.answer_msg_id = m.id),
                                m.priority, m.request_priority, m.request_ttl,
                                m.request_supersedes, m.request_dedup_idle, m.kind
                         FROM messages m WHERE m.idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if let Some(r) = it.next().await? {
                    let id = r.get::<i64>(0)?;
                    let request_matches = if record_request_tuple {
                        r.get::<Option<String>>(8)?.as_deref() == Some(request_priority.as_str())
                            && r.get::<Option<i64>>(9)? == Some(ttl)
                            && r.get::<Option<i64>>(10)? == Some(supersedes.unwrap_or(0))
                            && r.get::<Option<i64>>(11)? == Some(i64::from(dedup_idle))
                    } else {
                        r.get::<Option<String>>(8)?.as_deref() == Some(effective_priority.as_str())
                            && r.get::<Option<i64>>(9)? == Some(0)
                            && r.get::<Option<i64>>(10)? == Some(0)
                            && r.get::<Option<i64>>(11)? == Some(0)
                    };
                    let expected_kind = if record_request_tuple {
                        dedup_idle.then_some(crate::model::KIND_IDLE)
                    } else {
                        Some(crate::model::KIND_SESSION_PLAIN)
                    };
                    let same = r.get::<String>(1)? == sender
                        && r.get::<String>(2)? == recipient
                        && r.get::<Option<String>>(3)?.as_deref() == subject
                        && r.get::<String>(4)? == body
                        && r.get::<Option<i64>>(5)?.is_none()
                        && r.get::<i64>(6)? == 0
                        && r.get::<String>(7)? == effective_priority
                        && r.get::<Option<String>>(12)?.as_deref() == expected_kind
                        && request_matches;
                    drop(it);
                    if same {
                        return Ok((id, false));
                    }
                    anyhow::bail!(
                        "idempotency key is already associated with a different message."
                    );
                }
                drop(it);
            }
            if let Some(old_id) = supersedes {
                let mut rows = tx
                    .query(
                        "SELECT sender, recipient FROM messages WHERE id = ?1",
                        params(vec![old_id.into()]),
                    )
                    .await?;
                let old_sender = match rows.next().await? {
                    Some(row) => {
                        let old_sender = row.get::<String>(0)?;
                        let old_recipient = row.get::<String>(1)?;
                        if old_recipient != recipient {
                            anyhow::bail!(
                                "cannot supersede: #{old_id} was addressed to a different recipient"
                            );
                        }
                        old_sender
                    }
                    None => anyhow::bail!("cannot supersede: message #{old_id} does not exist"),
                };
                drop(rows);
                if old_sender != sender {
                    anyhow::bail!(
                        "cannot supersede: #{old_id} was sent by '{old_sender}', not '{sender}'"
                    );
                }
            }
            let ts = now();
            let expires_at = (ttl > 0).then(|| crate::model::expiry_from_ttl(ts, ttl));
            let (
                kind,
                stored_request_priority,
                stored_request_ttl,
                stored_request_supersedes,
                stored_request_dedup_idle,
            ) = if record_request_tuple {
                (
                    dedup_idle.then_some(crate::model::KIND_IDLE.to_string()),
                    Some(request_priority.clone()),
                    Some(ttl),
                    Some(supersedes.unwrap_or(0)),
                    Some(i64::from(dedup_idle)),
                )
            } else {
                (
                    Some(crate::model::KIND_SESSION_PLAIN.to_string()),
                    Some(effective_priority.clone()),
                    Some(0),
                    Some(0),
                    Some(0),
                )
            };
            tx.execute(
                "INSERT INTO messages
                        (ts, sender, recipient, subject, body, idempotency_key, trace_id,
                         priority, expires_at, request_priority, request_ttl,
                         request_supersedes, request_dedup_idle, kind) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params(vec![
                    ts.into(),
                    sender.into(),
                    recipient.into(),
                    subject.map(|s| s.to_string()).into(),
                    body.into(),
                    idempotency_key.map(|s| s.to_string()).into(),
                    trace_id.map(|s| s.to_string()).into(),
                    effective_priority.into(),
                    expires_at.into(),
                    stored_request_priority.into(),
                    stored_request_ttl.into(),
                    stored_request_supersedes.into(),
                    stored_request_dedup_idle.into(),
                    kind.into(),
                ]),
            )
            .await?;
            let id = self.conn.last_insert_rowid();
            if let Some(old_id) = supersedes {
                tx.execute(
                    "UPDATE messages SET superseded_by = ?2 WHERE id = ?1",
                    params(vec![old_id.into(), id.into()]),
                )
                .await?;
            }
            if dedup_idle && apply_dedup_effects {
                tx.execute(
                    "UPDATE messages SET superseded_by = ?1
                     WHERE sender = ?2 AND recipient = ?3
                       AND kind = ?4
                       AND superseded_by IS NULL
                       AND id <> ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM reads r
                           WHERE r.message_id = messages.id AND r.reader = ?3
                       )",
                    params(vec![
                        id.into(),
                        sender.into(),
                        recipient.into(),
                        crate::model::KIND_IDLE.into(),
                    ]),
                )
                .await?;
            }
            tx.commit().await?;
            Ok((id, true))
        })
    }

    fn message_by_idempotency_key(&self, key: &str) -> Result<Option<Message>> {
        if !crate::model::idempotency_key_valid(key) {
            anyhow::bail!("idempotency_key is invalid or too long.");
        }
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to,
                            idempotency_key, trace_id, priority, superseded_by, expires_at, kind,
                            request_priority, request_ttl, request_supersedes, request_dedup_idle
                     FROM messages WHERE idempotency_key = ?1",
                    params(vec![key.into()]),
                )
                .await?;
            match rows.next().await? {
                Some(row) => Ok(Some(row_to_message(&row)?)),
                None => Ok(None),
            }
        })
    }

    fn message_exists(&self, id: i64) -> Result<bool> {
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM messages WHERE id = ?1",
                    params(vec![id.into()]),
                )
                .await?;
            Ok(rows.next().await?.is_some())
        })
    }

    fn inbox(
        &self,
        me: &str,
        include_read: bool,
        mark_read: bool,
        limit: i64,
    ) -> Result<(Vec<Message>, i64)> {
        // `inbox` is a local-inbox op (never called on a foreign handle); only the
        // read-marking branch writes, so guard only when it would.
        if mark_read {
            self.guard_writable()?;
        }
        // WL-038: opportunistically delete expired ephemeral rows before the read.
        if mark_read {
            let _ = self.sweep_expired_messages();
        }
        let limit = clamp_limit(limit);
        let now_cut = now();
        self.rt.block_on(async {
            // WL-038: positional index 11 is `expires_at`; guard excludes
            // expired-but-not-yet-swept rows (the SqliteStore mirror).
            let sql = if include_read {
                format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind, request_priority, request_ttl, request_supersedes, request_dedup_idle FROM messages
                     WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                       AND superseded_by IS NULL
                       AND (expires_at IS NULL OR expires_at > ?3)
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                )
            } else {
                format!(
                    "SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to, m.idempotency_key, m.trace_id, m.priority, m.superseded_by, m.expires_at, m.kind FROM messages m
                     WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
                       AND m.superseded_by IS NULL
                       AND (m.expires_at IS NULL OR m.expires_at > ?3)
                       AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)
                     ORDER BY m.id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                )
            };

            // Run the SELECT, the read-marking, and the remaining count inside ONE
            // IMMEDIATE transaction so the returned rows, the marks, and
            // `remaining` are a single consistent snapshot — matching SqliteStore.
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;

            let mut rows_iter = tx
                .query(&sql, params(vec![me.into(), limit.into(), now_cut.into()]))
                .await?;
            let mut rows: Vec<Message> = Vec::new();
            while let Some(r) = rows_iter.next().await? {
                rows.push(row_to_message(&r)?);
            }
            drop(rows_iter);
            rows.reverse();

            if mark_read && !rows.is_empty() {
                let ts = now();
                for m in &rows {
                    tx.execute(
                        "INSERT OR IGNORE INTO reads (message_id, reader, ts) VALUES (?1,?2,?3)",
                        params(vec![m.id.into(), me.into(), ts.into()]),
                    )
                    .await?;
                }
            }

            let remaining = unread_count_tx(&tx, me).await?;
            tx.commit().await?;
            Ok((rows, remaining))
        })
    }

    fn unread_count(&self, me: &str) -> Result<i64> {
        check_ident("reader", me)?;
        self.rt.block_on(self.unread_count_async(me))
    }

    fn mark_message_read(&self, me: &str, message_id: i64) -> Result<bool> {
        self.guard_writable()?;
        check_ident("reader", me)?;
        if message_id <= 0 {
            return Ok(false);
        }
        let cutoff = now();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let eligible_sql = format!(
                "SELECT EXISTS(
                    SELECT 1 FROM messages
                    WHERE id = ?2
                      AND (recipient = ?1 OR recipient IN {bc})
                      AND sender != ?1
                      AND superseded_by IS NULL
                      AND (expires_at IS NULL OR expires_at > ?3)
                )",
                bc = BROADCAST_SQL
            );
            let eligible = {
                let mut rows = tx
                    .query(
                        &eligible_sql,
                        params(vec![me.into(), message_id.into(), cutoff.into()]),
                    )
                    .await?;
                let eligible = rows
                    .next()
                    .await?
                    .map(|row| row.get::<i64>(0))
                    .transpose()?
                    .unwrap_or(0)
                    != 0;
                drop(rows);
                eligible
            };
            if !eligible {
                tx.commit().await?;
                return Ok(false);
            }
            let insert_sql = format!(
                "INSERT OR IGNORE INTO reads (message_id, reader, ts)
                 SELECT id, ?1, ?3 FROM messages
                 WHERE id = ?2
                   AND (recipient = ?1 OR recipient IN {bc})
                   AND sender != ?1
                   AND superseded_by IS NULL
                   AND (expires_at IS NULL OR expires_at > ?3)",
                bc = BROADCAST_SQL
            );
            tx.execute(
                &insert_sql,
                params(vec![me.into(), message_id.into(), cutoff.into()]),
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so history never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let now_cut = now();
        self.rt.block_on(async {
            let mut rows: Vec<Message> = Vec::new();
            if let Some(p) = peer {
                let sql = format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind, request_priority, request_ttl, request_supersedes, request_dedup_idle FROM messages
                     WHERE ((sender = ?1 AND (recipient = ?2 OR recipient IN {bc}))
                        OR (sender = ?2 AND (recipient = ?1 OR recipient IN {bc})))
                       AND (expires_at IS NULL OR expires_at > ?4)
                     ORDER BY id DESC LIMIT ?3",
                    bc = BROADCAST_SQL
                );
                let mut it = self
                    .conn
                    .query(
                        &sql,
                        params(vec![me.into(), p.into(), limit.into(), now_cut.into()]),
                    )
                    .await?;
                while let Some(r) = it.next().await? {
                    rows.push(row_to_message(&r)?);
                }
            } else {
                let sql = format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind, request_priority, request_ttl, request_supersedes, request_dedup_idle FROM messages
                     WHERE (sender = ?1 OR recipient = ?1 OR recipient IN {bc})
                       AND (expires_at IS NULL OR expires_at > ?3)
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                );
                let mut it = self
                    .conn
                    .query(&sql, params(vec![me.into(), limit.into(), now_cut.into()]))
                    .await?;
                while let Some(r) = it.next().await? {
                    rows.push(row_to_message(&r)?);
                }
            };
            rows.reverse();
            Ok(rows)
        })
    }

    fn all_messages(&self, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so whole-DB export never surfaces expired rows.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let now_cut = now();
        self.rt.block_on(async {
            let sql = "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind, request_priority, request_ttl, request_supersedes, request_dedup_idle FROM messages
                 WHERE (expires_at IS NULL OR expires_at > ?2)
                 ORDER BY id DESC LIMIT ?1";
            let mut it = self
                .conn
                .query(sql, params(vec![limit.into(), now_cut.into()]))
                .await?;
            let mut rows: Vec<Message> = Vec::new();
            while let Some(r) = it.next().await? {
                rows.push(row_to_message(&r)?);
            }
            drop(it);
            rows.reverse();
            Ok(rows)
        })
    }

    fn search(&self, query: &str, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so search never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let now_cut = now();
        self.rt.block_on(async {
            let sql = "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind FROM messages
                 WHERE id IN (
                     SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2
                 )
                 AND (expires_at IS NULL OR expires_at > ?3)
                 ORDER BY id DESC LIMIT ?2";
            let mut it = self
                .conn
                .query(sql, params(vec![query.into(), limit.into(), now_cut.into()]))
                .await?;
            let mut rows: Vec<Message> = Vec::new();
            while let Some(r) = it.next().await? {
                rows.push(row_to_message(&r)?);
            }
            Ok(rows)
        })
    }

    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so the drain never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let now_cut = now();
        self.rt.block_on(async {
            let sql = format!(
                "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind FROM messages
                 WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1 AND id > ?2
                   AND superseded_by IS NULL
                   AND (expires_at IS NULL OR expires_at > ?4)
                 ORDER BY id ASC LIMIT ?3",
                bc = BROADCAST_SQL
            );
            let mut it = self
                .conn
                .query(
                    &sql,
                    params(vec![
                        me.into(),
                        since_id.into(),
                        limit.into(),
                        now_cut.into(),
                    ]),
                )
                .await?;
            let mut rows: Vec<Message> = Vec::new();
            while let Some(r) = it.next().await? {
                rows.push(row_to_message(&r)?);
            }
            Ok(rows)
        })
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        // SELECT-only (N+1 unread/last-activity); bounded on a read-only/remote handle.
        self.block_on_bounded(async {
            let mut names: Vec<String> = Vec::new();
            {
                let mut it = self
                    .conn
                    .query("SELECT DISTINCT sender FROM messages", ())
                    .await?;
                while let Some(r) = it.next().await? {
                    names.push(r.get::<String>(0)?);
                }
            }
            {
                let mut it = self
                    .conn
                    .query("SELECT DISTINCT recipient FROM messages", ())
                    .await?;
                while let Some(r) = it.next().await? {
                    let n = r.get::<String>(0)?;
                    if !is_broadcast(&n) {
                        names.push(n);
                    }
                }
            }
            names.sort();
            names.dedup();
            // Bound the per-name N+1 scan below (mirrors SqliteStore::sessions).
            names.truncate(MAX_SESSIONS);

            let mut out = Vec::new();
            for n in names {
                let unread = self.unread_count_async(&n).await?;
                let last: i64 = {
                    let mut it = self
                        .conn
                        .query(
                            "SELECT COALESCE(MAX(ts),0) FROM messages WHERE sender=?1 OR recipient=?1",
                            params(vec![n.as_str().into()]),
                        )
                        .await?;
                    match it.next().await? {
                        Some(r) => r.get::<i64>(0).unwrap_or(0),
                        None => 0,
                    }
                };
                out.push((n, unread, last));
            }
            Ok(out)
        })
    }

    fn total_messages(&self) -> Result<i64> {
        self.rt
            .block_on(async { self.total_messages_async().await })
    }

    fn snapshot_to(&self, dest: &std::path::Path) -> Result<()> {
        // A remote (Turso) backend has NO local file to vacuum-into a client-side
        // path; bail clearly rather than silently producing nothing. Snapshot the
        // Turso DB server-side instead.
        let _src = self.local_path.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "backup is not supported for the remote libsql backend \
                 (snapshot the Turso database server-side)"
            )
        })?;
        let dest_str = dest.to_str().ok_or_else(|| {
            anyhow::anyhow!("snapshot destination path is not valid UTF-8: {dest:?}")
        })?;
        // libSQL's bundled SQLite supports VACUUM INTO. The destination path is
        // BOUND as a parameter — never inlined — and must not already exist.
        self.rt
            .block_on(async {
                self.conn
                    .execute("VACUUM INTO ?1", params(vec![dest_str.into()]))
                    .await
            })
            .map_err(|e| anyhow::anyhow!("VACUUM INTO failed for {}: {e}", dest.display()))?;
        // Read-back verify (WL-041 spirit): the snapshot must re-open read-only and
        // be a valid weave store before we declare success.
        let snap = LibsqlStore::open_readonly(dest)
            .map_err(|e| anyhow::anyhow!("snapshot at {} did not re-open: {e}", dest.display()))?;
        snap.total_messages().map_err(|e| {
            anyhow::anyhow!(
                "snapshot at {} is not a valid weave store: {e}",
                dest.display()
            )
        })?;
        Ok(())
    }

    fn clear_inbox(&self, me: &str) -> Result<usize> {
        self.guard_writable()?;
        // Reuse `inbox` (which has its own runtime entry) to fetch the unread
        // set, then mark each read — same approach and semantics as SqliteStore.
        let (rows, _) = self.inbox(me, false, false, i64::MAX)?;
        self.rt.block_on(async {
            let ts = now();
            let tx = self.conn.transaction().await?;
            for m in &rows {
                tx.execute(
                    "INSERT OR IGNORE INTO reads (message_id, reader, ts) VALUES (?1,?2,?3)",
                    params(vec![m.id.into(), me.into(), ts.into()]),
                )
                .await?;
            }
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        })?;
        Ok(rows.len())
    }

    fn clear_all(&self) -> Result<i64> {
        self.guard_writable()?;
        self.rt.block_on(async {
            // Both deletes in ONE transaction so a crash can't orphan `reads`.
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let n: i64 = {
                let mut rows = tx.query("SELECT COUNT(*) FROM messages", ()).await?;
                match rows.next().await? {
                    Some(r) => r.get::<i64>(0)?,
                    None => 0,
                }
            };
            tx.execute("DELETE FROM summaries", ()).await?;
            tx.execute("DELETE FROM asks", ()).await?;
            tx.execute("DELETE FROM ask_groups", ()).await?;
            tx.execute("DELETE FROM messages", ()).await?;
            tx.execute("DELETE FROM reads", ()).await?;
            tx.execute("DELETE FROM wake_acks", ()).await?;
            tx.commit().await?;
            Ok(n)
        })
    }

    fn peek_oldest_unread(&self, me: &str) -> Result<Option<Message>> {
        // WL-038: opportunistic sweep so the wake hook never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        self.block_on_bounded(async { peek_oldest_unread_on(&self.conn, me).await })
    }

    fn wake_last_acked(&self, me: &str) -> Result<i64> {
        self.block_on_bounded(async { wake_last_acked_on(&self.conn, me).await })
    }

    fn set_wake_ack(&self, me: &str, id: i64) -> Result<()> {
        self.guard_writable()?;
        check_ident("peer name", me)?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO wake_acks (reader, last_id) VALUES (?1,?2)
                     ON CONFLICT(reader) DO UPDATE SET last_id=?2",
                    params(vec![me.into(), id.into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn gc(&self, older_than_secs: i64) -> Result<i64> {
        self.guard_writable()?;
        let cutoff = now().saturating_sub(older_than_secs.max(0));
        self.rt.block_on(async {
            // COUNT + both DELETEs in ONE IMMEDIATE transaction: accurate count and
            // no orphaned `reads` on a mid-operation crash (mirrors SqliteStore).
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let n: i64 = {
                let mut rows = tx
                    .query(
                        "SELECT COUNT(*) FROM messages WHERE ts < ?1",
                        params(vec![cutoff.into()]),
                    )
                    .await?;
                match rows.next().await? {
                    Some(r) => r.get::<i64>(0)?,
                    None => 0,
                }
            };
            // Clear only the effective public link before deleting its target;
            // request_supersedes remains as private keyed-replay metadata.
            let expiry_cut = now();
            tx.execute(
                "UPDATE asks SET reply_to = NULL
                  WHERE reply_to IN (
                        SELECT id FROM asks
                         WHERE question_msg_id IN (
                                   SELECT id FROM messages
                                    WHERE ts < ?1 OR (expires_at IS NOT NULL AND expires_at <= ?2))
                            OR answer_msg_id IN (
                                   SELECT id FROM messages
                                    WHERE ts < ?1 OR (expires_at IS NOT NULL AND expires_at <= ?2))
                  )",
                params(vec![cutoff.into(), expiry_cut.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM asks
                  WHERE question_msg_id IN (
                            SELECT id FROM messages
                             WHERE ts < ?1 OR (expires_at IS NOT NULL AND expires_at <= ?2))
                     OR answer_msg_id IN (
                            SELECT id FROM messages
                             WHERE ts < ?1 OR (expires_at IS NOT NULL AND expires_at <= ?2))",
                params(vec![cutoff.into(), expiry_cut.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM ask_groups
                  WHERE NOT EXISTS (SELECT 1 FROM asks WHERE asks.parent_id = ask_groups.parent_id)",
                (),
            )
            .await?;
            tx.execute(
                "UPDATE messages SET in_reply_to = NULL
                  WHERE in_reply_to IN (
                        SELECT id FROM messages
                         WHERE ts < ?1 OR (expires_at IS NOT NULL AND expires_at <= ?2)
                  )",
                params(vec![cutoff.into(), expiry_cut.into()]),
            )
            .await?;
            tx.execute(
                "UPDATE messages SET superseded_by = NULL
                  WHERE superseded_by IN (
                        SELECT id FROM messages
                         WHERE ts < ?1
                            OR (expires_at IS NOT NULL AND expires_at <= ?2)
                  )",
                params(vec![cutoff.into(), expiry_cut.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM reads WHERE message_id IN (SELECT id FROM messages WHERE ts < ?1)",
                params(vec![cutoff.into()]),
            )
            .await?;
            let retention_deleted = tx
                .execute(
                "DELETE FROM messages WHERE ts < ?1",
                params(vec![cutoff.into()]),
            )
            .await?;
            // WL-038: fold the ephemeral expiry into the SAME gc pass — delete expired
            // messages (and their reads) even if `ts >= cutoff` (delete-on-sweep),
            // mirroring SqliteStore::gc.
            tx.execute(
                "DELETE FROM reads WHERE message_id IN
                    (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
                params(vec![expiry_cut.into()]),
            )
            .await?;
            let expiry_deleted = tx
                .execute(
                    "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                    params(vec![expiry_cut.into()]),
                )
                .await?;
            // A summary represents the whole thread, so any removed root or
            // reply invalidates every cache entry. Keep invalidation atomic with
            // message deletion; the returned count remains `n` above.
            if retention_deleted > 0 || expiry_deleted > 0 {
                tx.execute("DELETE FROM summaries", ()).await?;
            }
            // P6: prune the delivery trace by the SAME cutoff so it is bounded by the
            // existing retention pass (no new sweeper). Mirrors SqliteStore::gc.
            tx.execute(
                "DELETE FROM delivery_log WHERE ts < ?1",
                params(vec![cutoff.into()]),
            )
            .await?;
            // WL-016: prune terminal schedule rows older than the retention cutoff.
            tx.execute(
                "DELETE FROM schedules WHERE created_ts < ?1 AND (cancelled = 1 OR executed_ts IS NOT NULL)",
                params(vec![cutoff.into()]),
            )
            .await?;
            tx.commit().await?;
            Ok(n)
        })
    }

    fn reply_target(&self, sender: &str, in_reply_to: i64) -> Result<(String, Option<String>)> {
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT sender, recipient, subject FROM messages WHERE id = ?1",
                    params(vec![in_reply_to.into()]),
                )
                .await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("message {in_reply_to} not found"))?;
            let psender = row.get::<String>(0)?;
            let precipient = row.get::<String>(1)?;
            let psubject = row.get::<Option<String>>(2)?;
            let recipient = if psender == sender {
                precipient
            } else {
                psender
            };
            Ok((recipient, reply_subject(psubject.as_deref())))
        })
    }

    fn set_in_reply_to(&self, message_id: i64, in_reply_to: i64) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE messages SET in_reply_to = ?1 WHERE id = ?2",
                    params(vec![in_reply_to.into(), message_id.into()]),
                )
                .await?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn reply_configured_idempotent(
        &self,
        sender: &str,
        in_reply_to: i64,
        body: &str,
        idempotency_key: Option<&str>,
        priority: Option<&str>,
        ttl: i64,
        subject_override: Option<&str>,
    ) -> Result<(i64, bool)> {
        // Resolve the parent, check an exact replay, and INSERT a single row
        // carrying in_reply_to inside ONE IMMEDIATE transaction.
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_body(body)?;
        crate::store::check_subject(subject_override)?;
        if idempotency_key.is_some_and(|key| !crate::model::idempotency_key_valid(key)) {
            anyhow::bail!("idempotency_key is invalid or too long.");
        }
        if ttl != 0 && !crate::model::ttl_valid(ttl) {
            anyhow::bail!(
                "ttl must be 0 or between 1 and {} seconds.",
                crate::model::MAX_MSG_TTL_SECS
            );
        }
        let priority = crate::model::MessagePriority::parse(priority.unwrap_or("normal"));
        let priority = priority.as_str().to_string();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;

            if let Some(key) = idempotency_key {
                let mut outbox = tx
                    .query(
                        "SELECT id FROM outbox WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if outbox.next().await?.is_some() {
                    anyhow::bail!(
                        "idempotency key is already associated with a cross-store intent."
                    );
                }
                drop(outbox);
                let mut rows = tx
                    .query(
                        "SELECT m.id, m.sender, m.recipient, m.subject, m.body, m.in_reply_to,
                                EXISTS(SELECT 1 FROM asks a
                                       WHERE a.question_msg_id = m.id OR a.answer_msg_id = m.id),
                                m.priority, m.request_priority, m.request_ttl
                         FROM messages m WHERE m.idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if let Some(row) = rows.next().await? {
                    let id = row.get::<i64>(0)?;
                    let old_subject = row.get::<Option<String>>(3)?;
                    let same = row.get::<String>(1)? == sender
                        && row.get::<String>(4)? == body
                        && row.get::<Option<i64>>(5)? == Some(in_reply_to)
                        && subject_override
                            .is_none_or(|subject| old_subject.as_deref() == Some(subject))
                        && row.get::<i64>(6)? == 0
                        && row.get::<String>(7)? == priority
                        && row.get::<Option<String>>(8)?.as_deref() == Some(priority.as_str())
                        && row.get::<Option<i64>>(9)? == Some(ttl);
                    drop(rows);
                    if same {
                        return Ok((id, false));
                    }
                    anyhow::bail!(
                        "idempotency key is already associated with a different message."
                    );
                }
                drop(rows);
            }

            // Parent lookup inside the transaction so it shares the snapshot with
            // the insert below.
            let (recipient, subject) = {
                let mut rows = tx
                    .query(
                        "SELECT sender, recipient, subject FROM messages WHERE id = ?1",
                        params(vec![in_reply_to.into()]),
                    )
                    .await?;
                let row = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("message {in_reply_to} not found"))?;
                let psender = row.get::<String>(0)?;
                let precipient = row.get::<String>(1)?;
                let psubject = row.get::<Option<String>>(2)?;
                let recipient = if psender == sender {
                    precipient
                } else {
                    psender
                };
                (
                    recipient,
                    subject_override
                        .map(str::to_string)
                        .or_else(|| reply_subject(psubject.as_deref())),
                )
            };

            let ts = now();
            let expires_at = (ttl > 0).then(|| crate::model::expiry_from_ttl(ts, ttl));
            tx.execute(
                "INSERT INTO messages
                    (ts, sender, recipient, subject, body, in_reply_to, idempotency_key,
                     priority, expires_at, request_priority, request_ttl) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params(vec![
                    ts.into(),
                    sender.into(),
                    recipient.into(),
                    subject.into(),
                    body.into(),
                    in_reply_to.into(),
                    idempotency_key.map(str::to_string).into(),
                    priority.clone().into(),
                    expires_at.into(),
                    priority.into(),
                    ttl.into(),
                ]),
            )
            .await?;
            let id = self.conn.last_insert_rowid();
            tx.commit().await?;
            Ok((id, true))
        })
    }

    fn thread(&self, root_id: i64, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let sql = "
                WITH RECURSIVE t(id) AS (
                    SELECT id FROM messages WHERE id = ?1
                    UNION
                    SELECT m.id FROM messages m JOIN t ON m.in_reply_to = t.id
                )
                SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to,
                       m.idempotency_key, m.trace_id, m.priority, m.superseded_by, m.expires_at, m.kind
                FROM messages m JOIN t ON m.id = t.id
                ORDER BY m.id ASC LIMIT ?2";
            let mut rows = self
                .conn
                .query(sql, params(vec![root_id.into(), limit.into()]))
                .await?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().await? {
                out.push(row_to_message(&r)?);
            }
            Ok(out)
        })
    }

    fn receipts(&self, message_id: i64) -> Result<Vec<(String, i64)>> {
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT reader, ts FROM reads WHERE message_id = ?1 ORDER BY ts ASC, reader ASC",
                    params(vec![message_id.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().await? {
                out.push((r.get::<String>(0)?, r.get::<i64>(1)?));
            }
            Ok(out)
        })
    }

    fn touch_peer(&self, name: &str) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE peers SET last_seen = ?1 WHERE name = ?2",
                    params(vec![now().into(), name.into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn register_peer_full(
        &self,
        name: &str,
        mux: &str,
        target: &str,
        socket: &str,
        cwd: Option<&str>,
        pid: Option<i64>,
        host: &str,
        repo: &str,
        branch: &str,
        worktree_id: &str,
        circle: &str,
        birth_cert: Option<&str>,
        client_session: &str,
    ) -> Result<String> {
        self.guard_writable()?;
        check_ident("peer name", name)?;
        if let Some(cert) = birth_cert {
            check_birth_cert(cert)?;
        }
        let repo = sanitize_tag(repo, MAX_REPO_LEN);
        let branch = sanitize_tag(branch, MAX_BRANCH_LEN);
        let worktree_id = sanitize_tag(worktree_id, MAX_WORKTREE_LEN);
        // Ownership keys are strict/lossless (mirrors sqlite); truncating one can
        // alias distinct sessions or break later lookup.
        let client_session = crate::store::client_session_key(client_session)?;
        let circle = if crate::model::circle_valid(circle) {
            circle.to_string()
        } else {
            crate::model::DEFAULT_CIRCLE.to_string()
        };
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let existing_cert: Option<Option<String>> = {
                let mut it = tx
                    .query(
                        "SELECT birth_cert FROM peers WHERE name = ?1",
                        params(vec![name.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some(r.get::<Option<String>>(0)?),
                    None => None,
                }
            };
            let cert = match existing_cert {
                None => {
                    // New peer: bind the SUPPLIED cert when one was given (WL-047 spawn
                    // pre-binds the parent-minted cert so the child's self-registration
                    // matches), else mint a fresh one. The supplied cert was validated
                    // above by `check_birth_cert`. All pre-WL-047 callers pass `None`,
                    // so this stays backward-compatible. (Mirrors the sqlite backend.)
                    let new_cert = match birth_cert {
                        Some(c) => c.to_string(),
                        None => mint_birth_cert()?,
                    };
                    tx.execute(
                        "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, birth_cert, client_session)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                        params(vec![
                            name.into(),
                            mux.into(),
                            target.into(),
                            socket.into(),
                            cwd.map(|s| s.to_string()).into(),
                            now().into(),
                            pid.into(),
                            host.into(),
                            repo.into(),
                            branch.into(),
                            worktree_id.into(),
                            circle.into(),
                            new_cert.clone().into(),
                            client_session.clone().into(),
                        ]),
                    )
                    .await?;
                    new_cert
                }
                Some(None) => {
                    // client_session preserve-on-empty (mirrors sqlite; trait doc).
                    let new_cert = mint_birth_cert()?;
                    tx.execute(
                        "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11, birth_cert=?12,
                                          client_session = CASE WHEN ?13 = '' THEN client_session ELSE ?13 END
                         WHERE name=?14",
                        params(vec![
                            mux.into(),
                            target.into(),
                            socket.into(),
                            cwd.map(|s| s.to_string()).into(),
                            now().into(),
                            pid.into(),
                            host.into(),
                            repo.into(),
                            branch.into(),
                            worktree_id.into(),
                            circle.into(),
                            new_cert.clone().into(),
                            client_session.clone().into(),
                            name.into(),
                        ]),
                    )
                    .await?;
                    new_cert
                }
                Some(Some(stored_cert)) => {
                    if let Some(supplied) = birth_cert {
                        if supplied != stored_cert {
                            anyhow::bail!("birth certificate mismatch for peer '{name}'");
                        }
                    } else {
                        anyhow::bail!("peer '{name}' already registered; provide --cert to re-register");
                    }
                    // client_session preserve-on-empty (mirrors sqlite; trait doc).
                    tx.execute(
                        "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11,
                                          client_session = CASE WHEN ?12 = '' THEN client_session ELSE ?12 END
                         WHERE name=?13",
                        params(vec![
                            mux.into(),
                            target.into(),
                            socket.into(),
                            cwd.map(|s| s.to_string()).into(),
                            now().into(),
                            pid.into(),
                            host.into(),
                            repo.into(),
                            branch.into(),
                            worktree_id.into(),
                            circle.into(),
                            client_session.clone().into(),
                            name.into(),
                        ]),
                    )
                    .await?;
                    stored_cert
                }
            };
            tx.commit().await?;
            Ok(cert)
        })
    }

    fn get_peer_by_client_session(&self, client_session: &str) -> Result<Option<Peer>> {
        let client_session = crate::store::client_session_key(client_session)?;
        if client_session.is_empty() {
            return Ok(None);
        }
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy, client_session FROM peers WHERE client_session=?1 ORDER BY last_seen DESC LIMIT 1",
                    params(vec![client_session.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => {
                    let mut p = row_to_peer(&r)?;
                    crate::model::expire_description(&mut p, now());
                    Ok(Some(p))
                }
                None => Ok(None),
            }
        })
    }

    fn get_birth_cert(&self, name: &str) -> Result<Option<String>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT birth_cert FROM peers WHERE name=?1",
                    params(vec![name.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => {
                    let cert: Option<String> = r.get(0)?;
                    Ok(cert)
                }
                None => Ok(None),
            }
        })
    }

    fn get_peer(&self, name: &str) -> Result<Option<Peer>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy, client_session FROM peers WHERE name=?1",
                    params(vec![name.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => {
                    let mut p = row_to_peer(&r)?;
                    // Read-time TTL: a stale description ages out to "" (daemon-free;
                    // the stored row is left untouched — pure read-time view).
                    crate::model::expire_description(&mut p, now());
                    Ok(Some(p))
                }
                None => Ok(None),
            }
        })
    }

    fn list_peers(&self) -> Result<Vec<Peer>> {
        // SELECT-only; bounded by the remote timeout on a read-only/remote handle.
        self.block_on_bounded(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy, client_session FROM peers ORDER BY name",
                    (),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_peer(&r)?);
            }
            // Read-time TTL: blank any stale description so every listing surface
            // treats it as absent (daemon-free; stored rows untouched).
            let now = now();
            for p in &mut out {
                crate::model::expire_description(p, now);
            }
            Ok(out)
        })
    }

    fn claim_orchestrator_role(
        &self,
        me: &str,
        circle: Option<&str>,
        force: bool,
    ) -> Result<ClaimOutcome> {
        self.guard_writable()?;
        check_ident("peer name", me)?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            // The caller must already be registered.
            let my_circle: Option<String> = {
                let mut it = tx
                    .query(
                        "SELECT circle FROM peers WHERE name=?1",
                        params(vec![me.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some(r.get::<String>(0)?),
                    None => None,
                }
            };
            let my_circle = match my_circle {
                Some(c) => c,
                None => anyhow::bail!("peer '{me}' is not registered"),
            };
            let target = match circle {
                Some(c) => crate::model::circle_or_default(c).to_string(),
                None => crate::model::circle_or_default(&my_circle).to_string(),
            };
            // Current orchestrators in the circle.
            let holders: Vec<Peer> = {
                let mut it = tx
                    .query(
                        "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy, client_session FROM peers WHERE role='orchestrator'",
                        (),
                    )
                    .await?;
                let mut out = Vec::new();
                while let Some(r) = it.next().await? {
                    let p = row_to_peer(&r)?;
                    if crate::model::circle_or_default(&p.circle) == target {
                        out.push(p);
                    }
                }
                out
            };
            // WL-019: co-orchestrator support.
            // Non-force claims are additive; force claims still steal.
            let mut demoted = Vec::new();
            if force {
                for p in &holders {
                    if p.name != me {
                        tx.execute(
                            "UPDATE peers SET role=?1 WHERE name=?2",
                            params(vec![
                                crate::model::PeerRole::Peer.as_str().into(),
                                p.name.clone().into(),
                            ]),
                        )
                        .await?;
                        demoted.push(p.name.clone());
                    }
                }
            }
            tx.execute(
                "UPDATE peers SET role=?1, circle=?2 WHERE name=?3",
                params(vec![
                    crate::model::PeerRole::Orchestrator.as_str().into(),
                    target.clone().into(),
                    me.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            demoted.sort();
            Ok(ClaimOutcome::Claimed {
                circle: target,
                demoted,
            })
        })
    }

    fn orchestrator_status(&self, circle: Option<&str>) -> Result<OrchestratorStatus> {
        let target = circle
            .map(crate::model::circle_or_default)
            .unwrap_or(crate::model::DEFAULT_CIRCLE)
            .to_string();
        // SELECT-only; bounded on a remote handle.
        self.block_on_bounded(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy, client_session FROM peers WHERE role='orchestrator'",
                    (),
                )
                .await?;
            let mut holders = Vec::new();
            while let Some(r) = it.next().await? {
                let p = row_to_peer(&r)?;
                if crate::model::circle_or_default(&p.circle) == target && is_alive(&p) {
                    holders.push(p);
                }
            }
            Ok(OrchestratorStatus {
                circle: target,
                present: !holders.is_empty(),
                holders,
            })
        })
    }

    fn set_turn_state(&self, name: &str, state: &str) -> Result<()> {
        self.guard_writable()?;
        check_ident("peer name", name)?;
        // Validate against the enum at the seam — an unknown value is a hard error,
        // never stored raw (mirrors the sqlite backend). The canonical label
        // re-derived from `as_str` is the only inlined turn_state SQL value.
        let canonical = crate::model::TurnState::from_str(state)
            .map_err(|e| anyhow::anyhow!(e))?
            .as_str();
        self.rt.block_on(async {
            // UPDATE-only on the caller's own row: never an INSERT, so a guessed
            // name worst-case touches 0 rows and no foreign row can be created.
            self.conn
                .execute(
                    "UPDATE peers SET turn_state=?2 WHERE name=?1",
                    params(vec![name.into(), canonical.into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn set_description(&self, name: &str, description: &str) -> Result<()> {
        self.guard_writable()?;
        check_ident("peer name", name)?;
        // Bound + control-strip at the store seam (lossy-but-total, mirrors the
        // sqlite backend). An oversized description truncates rather than errors.
        let clean = sanitize_tag(description, crate::model::MAX_DESC_LEN);
        // A cleared description stamps ts=0 (unambiguously "absent"); a set one
        // stamps now() so the read-time TTL can age it out independently of liveness.
        let ts = if clean.is_empty() { 0 } else { now() };
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE peers SET description=?2, description_ts=?3 WHERE name=?1",
                    params(vec![name.into(), clean.into(), ts.into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn enqueue_intent_idempotent(
        &self,
        to: &str,
        to_host: &str,
        from: &str,
        subject: Option<&str>,
        body: &str,
        sig: &str,
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
        priority: Option<&str>,
        ttl: i64,
    ) -> Result<(i64, bool)> {
        self.guard_writable()?;
        check_ident("recipient", to)?;
        check_ident("sender", from)?;
        crate::store::check_subject(subject)?;
        let to_host = to_host.trim();
        check_host(to_host)?;
        check_body(body)?;
        if idempotency_key.is_some_and(|key| !crate::model::idempotency_key_valid(key)) {
            anyhow::bail!("idempotency_key is invalid or too long.");
        }
        if trace_id.is_some_and(|id| !crate::model::trace_id_valid(id)) {
            anyhow::bail!("trace_id is invalid or too long.");
        }
        let p = crate::model::MessagePriority::parse(priority.unwrap_or("normal"))
            .as_str()
            .to_string();
        if ttl != 0 && !crate::model::ttl_valid(ttl) {
            anyhow::bail!(
                "ttl must be 0 or between 1 and {} seconds.",
                crate::model::MAX_MSG_TTL_SECS
            );
        }
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            if let Some(key) = idempotency_key {
                let mut messages = tx
                    .query(
                        "SELECT id FROM messages WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if messages.next().await?.is_some() {
                    anyhow::bail!(
                        "idempotency key is already associated with a local message."
                    );
                }
                drop(messages);
                let replay = {
                    let mut rows = tx
                        .query(
                            "SELECT id, to_peer, to_host, from_peer, subject, body, sig,
                                    priority, ttl
                             FROM outbox WHERE idempotency_key = ?1",
                            params(vec![key.into()]),
                        )
                        .await?;
                    match rows.next().await? {
                        Some(row) => Some((
                            row.get::<i64>(0)?,
                            row.get::<String>(1)?,
                            row.get::<String>(2)?,
                            row.get::<String>(3)?,
                            row.get::<Option<String>>(4)?,
                            row.get::<String>(5)?,
                            row.get::<String>(6)?,
                            row.get::<String>(7)?,
                            row.get::<i64>(8)?,
                        )),
                        None => None,
                    }
                };
                if let Some((
                    id,
                    old_to,
                    old_host,
                    old_from,
                    old_subject,
                    old_body,
                    _old_sig,
                    old_priority,
                    old_ttl,
                )) = replay
                {
                    if old_to == to
                        && old_host == to_host
                        && old_from == from
                        && old_subject.as_deref() == subject
                        && old_body == body
                        && old_priority == p
                        && old_ttl == ttl
                    {
                        return Ok((id, false));
                    }
                    anyhow::bail!(
                        "idempotency key is already associated with a different intent."
                    );
                }
            }
            tx
                .execute(
                    "INSERT INTO outbox (ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params(vec![
                        now().into(),
                        to.into(),
                        to_host.into(),
                        from.into(),
                        subject.map(|s| s.to_string()).into(),
                        body.into(),
                        sig.into(),
                        idempotency_key.map(|s| s.to_string()).into(),
                        trace_id.map(|s| s.to_string()).into(),
                        p.into(),
                        ttl.into(),
                    ]),
                )
                .await?;
            let id = self.conn.last_insert_rowid();
            tx.commit().await?;
            Ok((id, true))
        })
    }

    fn list_outbox(&self, for_recipient: &str, since_id: i64, limit: i64) -> Result<Vec<Intent>> {
        let limit = clamp_limit(limit);
        // SELECT-only; bounded by the remote timeout on a read-only/remote handle
        // (this is the call the pull path makes against the foreign source).
        self.block_on_bounded(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl FROM outbox \
                     WHERE to_peer = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3",
                    params(vec![for_recipient.into(), since_id.into(), limit.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_intent(&r)?);
            }
            Ok(out)
        })
    }

    fn outbox_all(&self, limit: i64) -> Result<Vec<Intent>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl FROM outbox \
                     ORDER BY id ASC LIMIT ?1",
                    params(vec![limit.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_intent(&r)?);
            }
            Ok(out)
        })
    }

    fn pull_cursor_get(&self, source: &str) -> Result<i64> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT last_id FROM pull_cursor WHERE source = ?1",
                    params(vec![source.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(r.get::<i64>(0)?),
                None => Ok(0),
            }
        })
    }

    fn pull_cursor_set(&self, source: &str, last_id: i64) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO pull_cursor (source, last_id) VALUES (?1, ?2) \
                     ON CONFLICT(source) DO UPDATE \
                     SET last_id = MAX(pull_cursor.last_id, excluded.last_id)",
                    params(vec![source.into(), last_id.into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn register_key(&self, identity: &str, pubkey: &str) -> Result<()> {
        self.guard_writable()?;
        check_ident("identity", identity)?;
        self.rt.block_on(async {
            // ADD semantics (#7): registering the SAME (identity,pubkey) again is a
            // no-op via `ON CONFLICT DO NOTHING`. Enforce the per-identity cap ONLY
            // for a genuinely NEW key — a duplicate never counts against it. Probe
            // existence + count BEFORE inserting so a duplicate short-circuits the
            // cap and never errors.
            let mut it = self
                .conn
                .query(
                    "SELECT EXISTS(SELECT 1 FROM identity_keys \
                     WHERE identity = ?1 AND pubkey = ?2)",
                    params(vec![identity.into(), pubkey.into()]),
                )
                .await?;
            let already: i64 = match it.next().await? {
                Some(r) => r.get::<i64>(0)?,
                None => 0,
            };
            if already == 0 {
                let mut cit = self
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM identity_keys WHERE identity = ?1",
                        params(vec![identity.into()]),
                    )
                    .await?;
                let count: i64 = match cit.next().await? {
                    Some(r) => r.get::<i64>(0)?,
                    None => 0,
                };
                if count as usize >= MAX_KEYS_PER_IDENT {
                    anyhow::bail!(
                        "identity '{identity}' already has the maximum {MAX_KEYS_PER_IDENT} \
                         registered keys; remove a retired one with `weave key remove` first"
                    );
                }
            }
            self.conn
                .execute(
                    "INSERT INTO identity_keys (identity, pubkey, added_ts) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(identity, pubkey) DO NOTHING",
                    params(vec![identity.into(), pubkey.into(), now().into()]),
                )
                .await?;
            Ok(())
        })
    }

    fn get_key(&self, identity: &str) -> Result<Option<String>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT pubkey FROM identity_keys WHERE identity = ?1 \
                     ORDER BY added_ts DESC, rowid DESC LIMIT 1",
                    params(vec![identity.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(r.get::<String>(0)?)),
                None => Ok(None),
            }
        })
    }

    fn get_keys(&self, identity: &str) -> Result<Vec<String>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT pubkey FROM identity_keys WHERE identity = ?1 \
                     ORDER BY added_ts ASC, rowid ASC",
                    params(vec![identity.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(r.get::<String>(0)?);
            }
            Ok(out)
        })
    }

    fn remove_key(&self, identity: &str, pubkey: &str) -> Result<bool> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "DELETE FROM identity_keys WHERE identity = ?1 AND pubkey = ?2",
                    params(vec![identity.into(), pubkey.into()]),
                )
                .await?;
            Ok(n > 0)
        })
    }

    fn list_keys(&self) -> Result<Vec<(String, String)>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT identity, pubkey FROM identity_keys \
                     ORDER BY identity, added_ts ASC, rowid ASC",
                    (),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push((r.get::<String>(0)?, r.get::<String>(1)?));
            }
            Ok(out)
        })
    }

    fn record_revocation(&self, ev: &RevocationEvent) -> Result<()> {
        // OWNER-ONLY-WRITES: trap on a read-only/foreign handle, like every other
        // write — the audit append only ever writes the LOCAL owned store.
        self.guard_writable()?;
        // Defensive clamp at the write seam (mirrors SqliteStore::record_revocation).
        let fp = clamp_field(&ev.fp);
        let identity = clamp_field(&ev.identity);
        let source = clamp_field(&ev.source);
        let kind = ev.kind.as_str().to_string();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO revocations (ts, fp, identity, source, kind) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params(vec![
                        ev.ts.into(),
                        fp.into(),
                        identity.into(),
                        source.into(),
                        kind.into(),
                    ]),
                )
                .await?;
            Ok(())
        })
    }

    fn list_revocations(&self, limit: i64) -> Result<Vec<RevocationEvent>> {
        let lim = limit.clamp(0, MAX_REVOCATIONS_LIST);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, ts, fp, identity, source, kind FROM revocations \
                     ORDER BY id DESC LIMIT ?1",
                    params(vec![lim.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(RevocationEvent {
                    id: r.get::<i64>(0)?,
                    ts: r.get::<i64>(1)?,
                    fp: r.get::<String>(2)?,
                    identity: r.get::<String>(3)?,
                    source: r.get::<String>(4)?,
                    kind: RevocationKind::parse(&r.get::<String>(5)?),
                });
            }
            Ok(out)
        })
    }

    fn count_revocations(&self) -> Result<i64> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query("SELECT COUNT(*) FROM revocations", ())
                .await?;
            match it.next().await? {
                Some(r) => Ok(r.get::<i64>(0)?),
                None => Ok(0),
            }
        })
    }

    fn ask_idempotent(
        &self,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        body: &str,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<(String, i64, bool)> {
        self.guard_writable()?;
        check_ident("asker", asker)?;
        check_ident("askee", askee)?;
        crate::store::check_subject(subject)?;
        check_body(body)?;
        if let Some(options) = options {
            check_body(options)?;
        }
        if is_broadcast(askee) {
            anyhow::bail!(
                "tracked ask is point-to-point; a broadcast askee is not supported (P1)."
            );
        }
        if let Some(rt) = reply_to {
            if !ask_id_valid(rt) {
                anyhow::bail!("invalid reply_to correlation id.");
            }
        }
        if idempotency_key.is_some_and(|key| !crate::model::idempotency_key_valid(key)) {
            anyhow::bail!("idempotency_key is invalid or too long.");
        }
        let ts = now();
        let subject_owned = subject.map(|s| s.to_string());
        let reply_to_owned = reply_to.map(|s| s.to_string());
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;

            if let Some(key) = idempotency_key {
                let mut outbox = tx
                    .query(
                        "SELECT id FROM outbox WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if outbox.next().await?.is_some() {
                    anyhow::bail!(
                        "idempotency key is already associated with a cross-store intent."
                    );
                }
                drop(outbox);
                let keyed_message = {
                    let mut rows = tx
                        .query(
                            "SELECT id, sender, recipient, subject, body, in_reply_to
                             FROM messages WHERE idempotency_key = ?1",
                            params(vec![key.into()]),
                        )
                        .await?;
                    match rows.next().await? {
                        Some(row) => Some((
                            row.get::<i64>(0)?,
                            row.get::<String>(1)?,
                            row.get::<String>(2)?,
                            row.get::<Option<String>>(3)?,
                            row.get::<String>(4)?,
                            row.get::<Option<i64>>(5)?,
                        )),
                        None => None,
                    }
                };
                if let Some((qid, old_asker, old_askee, _old_subject, old_body, _old_parent)) =
                    keyed_message
                {
                    let tracked = {
                        let mut rows = tx
                            .query(
                                "SELECT id, kind, options, reply_to,
                                        request_subject, request_subject_provided
                                 FROM asks WHERE question_msg_id = ?1",
                                params(vec![qid.into()]),
                            )
                            .await?;
                        match rows.next().await? {
                            Some(row) => Some((
                                row.get::<String>(0)?,
                                row.get::<String>(1)?,
                                row.get::<Option<String>>(2)?,
                                row.get::<Option<String>>(3)?,
                                row.get::<Option<String>>(4)?,
                                row.get::<Option<i64>>(5)?,
                            )),
                            None => None,
                        }
                    };
                    if let Some((
                        id,
                        old_kind,
                        old_options,
                        old_reply_to,
                        request_subject,
                        request_subject_provided,
                    )) = tracked
                    {
                        let subject_matches = request_subject_provided
                            == Some(i64::from(subject.is_some()))
                            && request_subject.as_deref() == subject;
                        if old_asker == asker
                            && old_askee == askee
                            && subject_matches
                            && old_body == body
                            && old_kind == kind.as_str()
                            && old_options.as_deref() == options
                            && old_reply_to.as_deref() == reply_to
                        {
                            return Ok((id, qid, false));
                        }
                    }
                    anyhow::bail!(
                        "idempotency key is already associated with a different message."
                    );
                }
            }

            // When chaining, load + close the prior thread and link the new question.
            let (in_reply_to, subject_final): (Option<i64>, Option<String>) =
                if let Some(rt) = &reply_to_owned {
                    let mut rows = tx
                        .query(
                            "SELECT asker, askee, state, question_msg_id, answer_msg_id
                             FROM asks WHERE id = ?1",
                            params(vec![rt.clone().into()]),
                        )
                        .await?;
                    let row = rows
                        .next()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("reply_to ask '{rt}' not found."))?;
                    let p_asker = row.get::<String>(0)?;
                    let p_askee = row.get::<String>(1)?;
                    let p_state = AskState::from_str(&row.get::<String>(2)?)
                        .map_err(|m| anyhow::anyhow!(m))?;
                    let p_qid = row.get::<i64>(3)?;
                    let p_aid = row.get::<Option<i64>>(4)?;
                    let same_pair = (p_asker == asker && p_askee == askee)
                        || (p_asker == askee && p_askee == asker);
                    if !same_pair {
                        anyhow::bail!("reply_to ask '{rt}' is between different parties.");
                    }
                    let link = p_aid.unwrap_or(p_qid);
                    if p_state != AskState::Acked {
                        if !p_state.can_transition(AskState::Acked) {
                            anyhow::bail!(
                                "cannot chain from ask '{rt}' in state {}.",
                                p_state.as_str()
                            );
                        }
                        tx.execute(
                            "UPDATE asks SET state = ?1, closed_ts = ?2, updated_ts = ?2 \
                             WHERE id = ?3",
                            params(vec![
                                AskState::Acked.as_str().into(),
                                ts.into(),
                                rt.clone().into(),
                            ]),
                        )
                        .await?;
                    }
                    // Inherit the prior subject's Re: discipline when none supplied.
                    let subj = match &subject_owned {
                        Some(s) => Some(s.clone()),
                        None => {
                            let mut sr = tx
                                .query(
                                    "SELECT subject FROM messages WHERE id = ?1",
                                    params(vec![link.into()]),
                                )
                                .await?;
                            let parent_subj = match sr.next().await? {
                                Some(r) => r.get::<Option<String>>(0)?,
                                None => None,
                            };
                            reply_subject(parent_subj.as_deref())
                        }
                    };
                    (Some(link), subj)
                } else {
                    (None, subject_owned.clone())
                };

            tx.execute(
                "INSERT INTO messages
                    (ts, sender, recipient, subject, body, in_reply_to, idempotency_key) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params(vec![
                    ts.into(),
                    asker.into(),
                    askee.into(),
                    subject_final.clone().into(),
                    body.into(),
                    in_reply_to.into(),
                    idempotency_key.map(str::to_string).into(),
                ]),
            )
            .await?;
            let question_msg_id = self.conn.last_insert_rowid();
            let id = new_ask_id(question_msg_id);
            // A plain `ask` is never part of a group: parent_id is NULL. Ask-many
            // children share this insert shape with a non-NULL parent_id.
            tx.execute(
                "INSERT INTO asks \
                    (id, question_msg_id, answer_msg_id, asker, askee, subject, \
                     request_subject, request_subject_provided, state, kind, options, reply_to, \
                     close_note, opened_ts, updated_ts, closed_ts, parent_id) \
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
                         NULL, ?12, ?12, NULL, NULL)",
                params(vec![
                    id.clone().into(),
                    question_msg_id.into(),
                    asker.into(),
                    askee.into(),
                    subject_final.into(),
                    subject_owned.clone().into(),
                    i64::from(subject_owned.is_some()).into(),
                    AskState::Open.as_str().into(),
                    kind.as_str().into(),
                    options.into(),
                    reply_to_owned.clone().into(),
                    ts.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok((id, question_msg_id, true))
        })
    }

    fn answer_idempotent(
        &self,
        responder: &str,
        correlation_id: &str,
        body: &str,
        idempotency_key: Option<&str>,
    ) -> Result<(i64, bool)> {
        self.guard_writable()?;
        check_ident("responder", responder)?;
        check_body(body)?;
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        if idempotency_key.is_some_and(|key| !crate::model::idempotency_key_valid(key)) {
            anyhow::bail!("idempotency_key is invalid or too long.");
        }
        let ts = now();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            if let Some(key) = idempotency_key {
                let mut outbox = tx
                    .query(
                        "SELECT id FROM outbox WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if outbox.next().await?.is_some() {
                    anyhow::bail!(
                        "idempotency key is already associated with a cross-store intent."
                    );
                }
                drop(outbox);
            }
            let (asker, askee, state, question_msg_id, existing_answer_id) = {
                let mut rows = tx
                    .query(
                        "SELECT asker, askee, state, question_msg_id, answer_msg_id
                         FROM asks WHERE id = ?1",
                        params(vec![correlation_id.into()]),
                    )
                    .await?;
                let row = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("ask '{correlation_id}' not found."))?;
                (
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<i64>(3)?,
                    row.get::<Option<i64>>(4)?,
                )
            };
            if responder != askee {
                anyhow::bail!("only the askee '{askee}' can answer ask '{correlation_id}'.");
            }
            if let Some(key) = idempotency_key {
                let replay = {
                    let mut rows = tx
                        .query(
                            "SELECT id, sender, recipient, body, in_reply_to
                             FROM messages WHERE idempotency_key = ?1",
                            params(vec![key.into()]),
                        )
                        .await?;
                    match rows.next().await? {
                        Some(row) => Some((
                            row.get::<i64>(0)?,
                            row.get::<String>(1)?,
                            row.get::<String>(2)?,
                            row.get::<String>(3)?,
                            row.get::<Option<i64>>(4)?,
                        )),
                        None => None,
                    }
                };
                if let Some((id, old_responder, old_recipient, old_body, old_parent)) = replay {
                    if existing_answer_id == Some(id)
                        && old_responder == responder
                        && old_recipient == asker
                        && old_body == body
                        && old_parent == Some(question_msg_id)
                    {
                        return Ok((id, false));
                    }
                    anyhow::bail!(
                        "idempotency key is already associated with a different message."
                    );
                }
            }
            let state = AskState::from_str(&state).map_err(|m| anyhow::anyhow!(m))?;
            if !state.can_transition(AskState::Answered) {
                anyhow::bail!(
                    "ask '{correlation_id}' is {} and cannot be answered.",
                    state.as_str()
                );
            }
            let parent_subject = {
                let mut sr = tx
                    .query(
                        "SELECT subject FROM messages WHERE id = ?1",
                        params(vec![question_msg_id.into()]),
                    )
                    .await?;
                match sr.next().await? {
                    Some(r) => r.get::<Option<String>>(0)?,
                    None => None,
                }
            };
            let subject = reply_subject(parent_subject.as_deref());
            tx.execute(
                "INSERT INTO messages
                    (ts, sender, recipient, subject, body, in_reply_to, idempotency_key) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params(vec![
                    ts.into(),
                    responder.into(),
                    asker.into(),
                    subject.into(),
                    body.into(),
                    question_msg_id.into(),
                    idempotency_key.map(str::to_string).into(),
                ]),
            )
            .await?;
            let answer_msg_id = self.conn.last_insert_rowid();
            tx.execute(
                "UPDATE asks SET answer_msg_id = ?1, state = ?2, updated_ts = ?3 WHERE id = ?4",
                params(vec![
                    answer_msg_id.into(),
                    AskState::Answered.as_str().into(),
                    ts.into(),
                    correlation_id.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok((answer_msg_id, true))
        })
    }

    fn ack(&self, acker: &str, correlation_id: &str, message: Option<&str>) -> Result<()> {
        self.guard_writable()?;
        check_ident("acker", acker)?;
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        if let Some(m) = message {
            check_body(m)?;
        }
        let ts = now();
        let message_owned = message.map(|s| s.to_string());
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let (askee, state) = {
                let mut rows = tx
                    .query(
                        "SELECT askee, state FROM asks WHERE id = ?1",
                        params(vec![correlation_id.into()]),
                    )
                    .await?;
                let row = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("ask '{correlation_id}' not found."))?;
                (row.get::<String>(0)?, row.get::<String>(1)?)
            };
            if acker != askee {
                anyhow::bail!("only the askee '{askee}' can ack ask '{correlation_id}'.");
            }
            let state = AskState::from_str(&state).map_err(|m| anyhow::anyhow!(m))?;
            if !state.can_transition(AskState::Acked) {
                anyhow::bail!(
                    "ask '{correlation_id}' is already {} (cannot ack).",
                    state.as_str()
                );
            }
            tx.execute(
                "UPDATE asks SET state = ?1, close_note = ?2, closed_ts = ?3, updated_ts = ?3 \
                 WHERE id = ?4",
                params(vec![
                    AskState::Acked.as_str().into(),
                    message_owned.into(),
                    ts.into(),
                    correlation_id.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn get_ask(&self, correlation_id: &str) -> Result<Option<Ask>> {
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                            options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id \
                     FROM asks WHERE id = ?1",
                    params(vec![correlation_id.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(row_to_ask(&r)?)),
                None => Ok(None),
            }
        })
    }

    fn list_asks(&self, me: &str, role: AskRole, limit: i64) -> Result<Vec<Ask>> {
        check_ident("me", me)?;
        let limit = clamp_limit(limit);
        let where_clause = match role {
            AskRole::Asker => "asker = ?1",
            AskRole::Askee => "askee = ?1",
            AskRole::Any => "(asker = ?1 OR askee = ?1)",
        };
        let sql = format!(
            "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                    options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id \
             FROM asks WHERE {where_clause} \
             ORDER BY opened_ts DESC, rowid DESC LIMIT ?2"
        );
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(&sql, params(vec![me.into(), limit.into()]))
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_ask(&r)?);
            }
            Ok(out)
        })
    }

    fn list_ask_groups(&self, parent_ids: &[String]) -> Result<Vec<AskGroup>> {
        self.rt.block_on(async {
            let mut out = Vec::new();
            // Bounded, parameterized per-id lookups (no IN-list interpolation).
            for pid in parent_ids {
                if !ask_many_id_valid(pid) {
                    continue;
                }
                let mut it = self
                    .conn
                    .query(
                        "SELECT parent_id, asker, subject, body, opened_ts, target_count \
                         FROM ask_groups WHERE parent_id = ?1",
                        params(vec![pid.clone().into()]),
                    )
                    .await?;
                if let Some(r) = it.next().await? {
                    out.push(AskGroup {
                        parent_id: r.get::<String>(0)?,
                        asker: r.get::<String>(1)?,
                        subject: r.get::<Option<String>>(2)?,
                        body: r.get::<String>(3)?,
                        opened_ts: r.get::<i64>(4)?,
                        target_count: r.get::<i64>(5)?,
                    });
                }
            }
            Ok(out)
        })
    }

    fn list_ask_group_children(&self, parent_id: &str) -> Result<Vec<Ask>> {
        if !ask_many_id_valid(parent_id) {
            anyhow::bail!("invalid ask-many parent id.");
        }
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                            options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id \
                       FROM asks WHERE parent_id = ?1 ORDER BY rowid ASC LIMIT ?2",
                    params(vec![
                        parent_id.into(),
                        (crate::store::MAX_ASK_MANY_TARGETS as i64 + 1).into(),
                    ]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await? {
                out.push(row_to_ask(&row)?);
            }
            Ok(out)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn import_ask(
        &self,
        id: &str,
        question_msg_id: i64,
        answer_msg_id: Option<i64>,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        state: AskState,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
        close_note: Option<&str>,
        opened_ts: i64,
        updated_ts: i64,
        closed_ts: Option<i64>,
        parent_id: Option<&str>,
    ) -> Result<bool> {
        let source_timestamps = self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT ts FROM messages WHERE id = ?1",
                    params(vec![question_msg_id.into()]),
                )
                .await?;
            let question = rows
                .next()
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("imported ask question message #{question_msg_id} is missing")
                })?
                .get::<i64>(0)?;
            drop(rows);
            let answer = match answer_msg_id {
                Some(answer_id) => {
                    let mut rows = self
                        .conn
                        .query(
                            "SELECT ts FROM messages WHERE id = ?1",
                            params(vec![answer_id.into()]),
                        )
                        .await?;
                    let answer = rows
                        .next()
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("imported ask answer message #{answer_id} is missing")
                        })?
                        .get::<i64>(0)?;
                    Some(answer)
                }
                None => None,
            };
            Ok::<_, anyhow::Error>(crate::store::ImportedAskSourceTimestamps { question, answer })
        })?;
        self.import_ask_with_source_timestamps(
            id,
            question_msg_id,
            answer_msg_id,
            asker,
            askee,
            subject,
            state,
            kind,
            options,
            reply_to,
            close_note,
            source_timestamps,
            opened_ts,
            updated_ts,
            closed_ts,
            parent_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn import_ask_with_source_timestamps(
        &self,
        id: &str,
        question_msg_id: i64,
        answer_msg_id: Option<i64>,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        state: AskState,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
        close_note: Option<&str>,
        source_timestamps: crate::store::ImportedAskSourceTimestamps,
        opened_ts: i64,
        updated_ts: i64,
        closed_ts: Option<i64>,
        parent_id: Option<&str>,
    ) -> Result<bool> {
        self.guard_writable()?;
        // Defense-in-depth re-validation at the store seam (mirrors the sqlite impl).
        check_ident("asker", asker)?;
        check_ident("askee", askee)?;
        crate::store::check_subject(subject)?;
        if !ask_id_valid(id) {
            anyhow::bail!("invalid imported ask id.");
        }
        if let Some(o) = options {
            check_body(o)?;
        }
        if let Some(c) = close_note {
            check_body(c)?;
        }
        if let Some(rt) = reply_to {
            if !ask_id_valid(rt) {
                anyhow::bail!("invalid imported reply_to correlation id.");
            }
        }
        if let Some(p) = parent_id {
            if !ask_many_id_valid(p) {
                anyhow::bail!("invalid imported parent_id.");
            }
        }
        crate::store::validate_imported_ask_lifecycle(
            question_msg_id,
            answer_msg_id,
            state,
            close_note,
            opened_ts,
            updated_ts,
            closed_ts,
        )?;
        crate::store::validate_imported_ask_source_timestamps(
            answer_msg_id,
            state,
            source_timestamps,
            opened_ts,
            updated_ts,
        )?;
        // Owned copies for the async move.
        let id = id.to_string();
        let asker = asker.to_string();
        let askee = askee.to_string();
        let subject = subject.map(|s| s.to_string());
        let options = options.map(|s| s.to_string());
        let reply_to = reply_to.map(|s| s.to_string());
        let close_note = close_note.map(|s| s.to_string());
        let parent_id = parent_id.map(|s| s.to_string());
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            // Idempotency: dedup on the remapped (asker, askee, question) triple — the
            // source ask id is meaningless across instances (mirrors the sqlite impl).
            let mut probe = tx
                .query(
                    "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                            options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id \
                     FROM asks WHERE asker = ?1 AND askee = ?2 AND question_msg_id = ?3",
                    params(vec![
                        asker.clone().into(),
                        askee.clone().into(),
                        question_msg_id.into(),
                    ]),
                )
                .await?;
            let existing = match probe.next().await? {
                Some(row) => Some(row_to_ask(&row)?),
                None => None,
            };
            drop(probe);
            if let Some(existing) = existing.as_ref() {
                if existing.answer_msg_id != answer_msg_id
                    || existing.subject.as_deref() != subject.as_deref()
                    || existing.state != state
                    || existing.kind != kind
                    || existing.options.as_deref() != options.as_deref()
                    || existing.reply_to.as_deref() != reply_to.as_deref()
                    || existing.close_note.as_deref() != close_note.as_deref()
                    || existing.opened_ts != opened_ts
                    || existing.updated_ts != updated_ts
                    || existing.closed_ts != closed_ts
                    || existing.parent_id.as_deref() != parent_id.as_deref()
                {
                    anyhow::bail!(
                        "imported ask thread belongs to different content or lifecycle state"
                    );
                }
            }
            validate_import_ask_relations_libsql(
                &tx,
                existing.as_ref().map(|ask| ask.id.as_str()),
                question_msg_id,
                answer_msg_id,
                &asker,
                &askee,
                subject.as_deref(),
                kind,
                options.as_deref(),
                reply_to.as_deref(),
                opened_ts,
                parent_id.as_deref(),
            )
            .await?;
            if existing.is_some() {
                tx.commit().await?;
                return Ok(false);
            }
            // 15-column POSITIONAL INSERT. The column order MUST match `row_to_ask`'s
            // positional indices 0..14 (id, question_msg_id, answer_msg_id, asker,
            // askee, subject, state, kind, options, reply_to, close_note, opened_ts,
            // updated_ts, closed_ts, parent_id) — libsql has no by-name binding.
            tx.execute(
                "INSERT INTO asks \
                    (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                     options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params(vec![
                    id.into(),
                    question_msg_id.into(),
                    answer_msg_id.into(),
                    asker.into(),
                    askee.into(),
                    subject.into(),
                    state.as_str().into(),
                    kind.as_str().into(),
                    options.into(),
                    reply_to.into(),
                    close_note.into(),
                    opened_ts.into(),
                    updated_ts.into(),
                    closed_ts.into(),
                    parent_id.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    fn import_ask_group(
        &self,
        parent_id: &str,
        asker: &str,
        subject: Option<&str>,
        body: &str,
        opened_ts: i64,
        target_count: i64,
    ) -> Result<bool> {
        self.guard_writable()?;
        if !ask_many_id_valid(parent_id) {
            anyhow::bail!("invalid imported ask-many parent id.");
        }
        check_ident("asker", asker)?;
        crate::store::check_subject(subject)?;
        check_body(body)?;
        if !(1..=crate::store::MAX_ASK_MANY_TARGETS as i64).contains(&target_count) {
            anyhow::bail!(
                "imported ask-many target_count must be between 1 and {}.",
                crate::store::MAX_ASK_MANY_TARGETS
            );
        }
        let parent_id = parent_id.to_string();
        let asker = asker.to_string();
        let subject = subject.map(|s| s.to_string());
        let body = body.to_string();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let mut probe = tx
                .query(
                    "SELECT asker, subject, body, opened_ts, target_count
                       FROM ask_groups WHERE parent_id = ?1",
                    params(vec![parent_id.clone().into()]),
                )
                .await?;
            let existing = match probe.next().await? {
                Some(r) => Some((
                    r.get::<String>(0)?,
                    r.get::<Option<String>>(1)?,
                    r.get::<String>(2)?,
                    r.get::<i64>(3)?,
                    r.get::<i64>(4)?,
                )),
                None => None,
            };
            drop(probe);
            if let Some((old_asker, old_subject, old_body, old_opened_ts, old_target_count)) =
                existing
            {
                if old_asker != asker
                    || old_subject.as_deref() != subject.as_deref()
                    || old_body != body
                    || old_opened_ts != opened_ts
                    || old_target_count != target_count
                {
                    anyhow::bail!("imported ask-many parent id belongs to different content");
                }
                tx.commit().await?;
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO ask_groups (parent_id, asker, subject, body, opened_ts, target_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params(vec![
                    parent_id.into(),
                    asker.into(),
                    subject.into(),
                    body.into(),
                    opened_ts.into(),
                    target_count.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    fn has_open_asks(&self, me: &str) -> Result<bool> {
        check_ident("me", me)?;
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM asks WHERE askee = ?1 AND state = 'open'",
                    params(vec![me.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => {
                    let count: i64 = r.get(0)?;
                    Ok(count > 0)
                }
                None => Ok(false),
            }
        })
    }

    fn ask_for_message(&self, message_id: i64) -> Result<Option<String>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id FROM asks WHERE question_msg_id = ?1 OR answer_msg_id = ?1 LIMIT 1",
                    params(vec![message_id.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(r.get::<String>(0)?)),
                None => Ok(None),
            }
        })
    }

    fn create_ask_many(
        &self,
        asker: &str,
        peers: &[String],
        subject: Option<&str>,
        body: &str,
    ) -> Result<AskManyOutcome> {
        // OWNER-ONLY-WRITES: trap a foreign/read-only handle BEFORE any validation or
        // insert, mirroring every other write method (the first statement).
        self.guard_writable()?;
        check_ident("asker", asker)?;
        crate::store::check_subject(subject)?;
        check_body(body)?;
        if is_broadcast(asker) {
            anyhow::bail!("the ask-many asker must be a concrete peer, not a broadcast alias.");
        }
        // De-dup the requested peer list (order-preserving); this is the canonical
        // post-dedup target_count.
        let mut deduped: Vec<String> = Vec::new();
        for p in peers {
            let t = p.trim();
            if !t.is_empty() && !deduped.iter().any(|d| d == t) {
                deduped.push(t.to_string());
            }
        }
        if deduped.is_empty() {
            anyhow::bail!("ask-many requires at least one target peer.");
        }
        if deduped.len() > MAX_ASK_MANY_TARGETS {
            anyhow::bail!(
                "ask-many targets {} peers; max {MAX_ASK_MANY_TARGETS} per fanout.",
                deduped.len()
            );
        }

        let ts = now();
        let parent_id = new_ask_many_id(ts);
        let subject_owned = subject.map(|s| s.to_string());
        let body_owned = body.to_string();
        let asker_owned = asker.to_string();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            // Parent anchor first; `target_count` is the de-duped REQUESTED count so
            // totality holds even when some children fail pre-insert.
            tx.execute(
                "INSERT INTO ask_groups (parent_id, asker, subject, body, opened_ts, target_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params(vec![
                    parent_id.clone().into(),
                    asker_owned.clone().into(),
                    subject_owned.clone().into(),
                    body_owned.clone().into(),
                    ts.into(),
                    (deduped.len() as i64).into(),
                ]),
            )
            .await?;

            let mut children: Vec<(String, std::result::Result<String, String>)> =
                Vec::with_capacity(deduped.len());
            for peer in &deduped {
                // Best-effort per child: a rejected peer records an error and is skipped
                // (no child ask), never aborting the whole fanout.
                if let Err(err) = check_ident("askee", peer) {
                    children.push((peer.clone(), Err(format!("{err}"))));
                    continue;
                }
                if is_broadcast(peer) {
                    children.push((
                        peer.clone(),
                        Err(
                            "broadcast alias cannot be an ask-many target (P2 takes an explicit \
                             peer list; a circle is P4)."
                                .to_string(),
                        ),
                    ));
                    continue;
                }
                tx.execute(
                    "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to) \
                     VALUES (?1,?2,?3,?4,?5,NULL)",
                    params(vec![
                        ts.into(),
                        asker_owned.clone().into(),
                        peer.clone().into(),
                        subject_owned.clone().into(),
                        body_owned.clone().into(),
                    ]),
                )
                .await?;
                let question_msg_id = self.conn.last_insert_rowid();
                let cid = new_ask_id(question_msg_id);
                // Same insert shape as the plain `ask`, with the parent_id stamped.
                tx.execute(
                    "INSERT INTO asks \
                        (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                         options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id) \
                     VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, ?9, ?9, NULL, ?10)",
                    params(vec![
                        cid.clone().into(),
                        question_msg_id.into(),
                        asker_owned.clone().into(),
                        peer.clone().into(),
                        subject_owned.clone().into(),
                        AskState::Open.as_str().into(),
                        AskKind::FreeText.as_str().into(),
                        Value::Null,
                        ts.into(),
                        parent_id.clone().into(),
                    ]),
                )
                .await?;
                children.push((peer.clone(), Ok(cid)));
            }
            tx.commit().await?;
            Ok(AskManyOutcome {
                parent_id: parent_id.clone(),
                children,
            })
        })
    }

    fn ask_many_result(
        &self,
        parent_id: &str,
        age_threshold: Option<i64>,
    ) -> Result<Option<AskManyResult>> {
        if !ask_many_id_valid(parent_id) {
            anyhow::bail!("invalid ask-many parent id.");
        }
        self.rt.block_on(async {
            let group: Option<AskGroup> = {
                let mut it = self
                    .conn
                    .query(
                        "SELECT parent_id, asker, subject, body, opened_ts, target_count \
                         FROM ask_groups WHERE parent_id = ?1",
                        params(vec![parent_id.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some(AskGroup {
                        parent_id: r.get::<String>(0)?,
                        asker: r.get::<String>(1)?,
                        subject: r.get::<Option<String>>(2)?,
                        body: r.get::<String>(3)?,
                        opened_ts: r.get::<i64>(4)?,
                        target_count: r.get::<i64>(5)?,
                    }),
                    None => None,
                }
            };
            let Some(group) = group else {
                return Ok(None);
            };
            let mut it = self
                .conn
                .query(
                    "SELECT id, askee, state, answer_msg_id FROM asks \
                     WHERE parent_id = ?1 ORDER BY opened_ts ASC, rowid ASC",
                    params(vec![parent_id.into()]),
                )
                .await?;
            let mut children: Vec<AskManyChildView> = Vec::new();
            let (mut answered, mut acked, mut pending) = (0i64, 0i64, 0i64);
            while let Some(r) = it.next().await? {
                let cid = r.get::<String>(0)?;
                let askee = r.get::<String>(1)?;
                let state =
                    AskState::from_str(&r.get::<String>(2)?).map_err(|m| anyhow::anyhow!(m))?;
                let answer_msg_id = r.get::<Option<i64>>(3)?;
                match state {
                    AskState::Open => pending += 1,
                    AskState::Answered => answered += 1,
                    AskState::Acked => acked += 1,
                }
                children.push(AskManyChildView {
                    peer: askee,
                    correlation_id: Some(cid),
                    state: Some(state),
                    answer_msg_id,
                    error: None,
                });
            }
            let created = children.len() as i64;
            let failed = (group.target_count - created).max(0);
            let total = group.target_count;
            let age_secs = Some(now() - group.opened_ts);
            let state = classify_ask_many(total, pending, failed, age_secs, age_threshold);
            Ok(Some(AskManyResult {
                parent_id: group.parent_id,
                asker: group.asker,
                subject: group.subject,
                body: group.body,
                opened_ts: group.opened_ts,
                target_count: group.target_count,
                total,
                answered,
                acked,
                pending,
                failed,
                state,
                children,
            }))
        })
    }

    // ── P3 job board (poll-only) ──────────────────────────────────────────────
    fn create_job(&self, creator: &str, spec: JobSpec) -> Result<Job> {
        self.guard_writable()?;
        validate_job_spec(creator, &spec)?;
        let ts = now();
        let owner = spec.owner.clone().unwrap_or_else(|| creator.to_string());
        let kind = spec.kind.clone().unwrap_or_else(|| "general".to_string());
        let visibility = spec
            .visibility
            .clone()
            .unwrap_or_else(|| "circle".to_string());
        let job_id = new_job_id(ts);
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO jobs (
                        id, title, description, kind, state, prompt, progress_events_json,
                        creator, owner, assignee, circle, correlation_id, source_kind,
                        source_id, scope, visibility, deadline_at, expires_at,
                        result_json, error_json, artifacts_json, cancel_requested,
                        opened_ts, updated_ts
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                        ?15, ?16, ?17, '{}', '{}', '[]', 0, ?18, ?18
                     )",
                    params(vec![
                        job_id.clone().into(),
                        spec.title.clone().into(),
                        spec.description.clone().unwrap_or_default().into(),
                        kind.into(),
                        JobState::Queued.as_str().into(),
                        spec.prompt.clone().into(),
                        creator.into(),
                        owner.into(),
                        spec.assignee.clone().into(),
                        spec.circle.clone().into(),
                        spec.correlation_id.clone().into(),
                        spec.source_kind.clone().into(),
                        spec.source_id.clone().into(),
                        spec.scope.clone().into(),
                        visibility.into(),
                        spec.deadline_at.into(),
                        spec.expires_at.into(),
                        ts.into(),
                    ]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.get_job(&job_id)?
            .ok_or_else(|| anyhow::anyhow!("job '{job_id}' vanished after insert."))
    }

    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        let sql = format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1");
        self.rt.block_on(async {
            let mut it = self.conn.query(&sql, params(vec![id.into()])).await?;
            match it.next().await? {
                Some(r) => Ok(Some(row_to_job(&r)?)),
                None => Ok(None),
            }
        })
    }

    fn list_jobs(&self, filter: JobFilter, limit: i64) -> Result<Vec<Job>> {
        let limit = clamp_limit(limit);
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Value> = Vec::new();
        if let Some(s) = filter.state {
            clauses.push(format!("state = ?{}", binds.len() + 1));
            binds.push(s.as_str().into());
        }
        if let Some(ref v) = filter.owner {
            clauses.push(format!("owner = ?{}", binds.len() + 1));
            binds.push(v.clone().into());
        }
        if let Some(ref v) = filter.creator {
            clauses.push(format!("creator = ?{}", binds.len() + 1));
            binds.push(v.clone().into());
        }
        if let Some(ref v) = filter.assignee {
            clauses.push(format!("assignee = ?{}", binds.len() + 1));
            binds.push(v.clone().into());
        }
        if let Some(ref v) = filter.circle {
            clauses.push(format!("circle = ?{}", binds.len() + 1));
            binds.push(v.clone().into());
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT {JOB_COLS} FROM jobs {where_sql} \
             ORDER BY updated_ts DESC, rowid DESC LIMIT ?{}",
            binds.len() + 1
        );
        binds.push(limit.into());
        self.rt.block_on(async {
            let mut it = self.conn.query(&sql, params(binds)).await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_job(&r)?);
            }
            Ok(out)
        })
    }

    fn claim_job(&self, id: &str, assignee: &str) -> Result<Option<Job>> {
        self.guard_writable()?;
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        check_ident("assignee", assignee)?;
        let ts = now();
        let did = self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let state_str = {
                let mut it = tx
                    .query(
                        "SELECT state FROM jobs WHERE id = ?1",
                        params(vec![id.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some(r.get::<String>(0)?),
                    None => None,
                }
            };
            let Some(state_str) = state_str else {
                tx.commit().await?;
                return Ok::<bool, anyhow::Error>(false);
            };
            let state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
            if state.is_terminal() {
                anyhow::bail!("job '{id}' is {} and cannot be claimed.", state.as_str());
            }
            let attempt_id = new_attempt_id(ts);
            tx.execute(
                "UPDATE jobs SET assignee = ?1, attempt_id = ?2, state = ?3, updated_ts = ?4 \
                 WHERE id = ?5",
                params(vec![
                    assignee.into(),
                    attempt_id.into(),
                    JobState::Running.as_str().into(),
                    ts.into(),
                    id.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })?;
        if did {
            self.get_job(id)
        } else {
            Ok(None)
        }
    }

    fn claim_queued_job(&self, id: &str, assignee: &str) -> Result<Option<Job>> {
        self.guard_writable()?;
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        check_ident("assignee", assignee)?;
        let ts = now();
        let attempt_id = new_attempt_id(ts);
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let changed = tx
                .execute(
                    "UPDATE jobs SET assignee = ?1, attempt_id = ?2, state = ?3, updated_ts = ?4 \
                     WHERE id = ?5 AND state = ?6 AND (assignee IS NULL OR assignee = ?1)",
                    params(vec![
                        assignee.into(),
                        attempt_id.into(),
                        JobState::Running.as_str().into(),
                        ts.into(),
                        id.into(),
                        JobState::Queued.as_str().into(),
                    ]),
                )
                .await?;
            if changed == 0 {
                tx.commit().await?;
                return Ok::<Option<Job>, anyhow::Error>(None);
            }
            // Read the row/token inside the transaction. Once commit succeeds,
            // the caller already owns the complete claim and cannot lose it to a
            // separate post-commit read failure.
            let sql = format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1");
            let claimed = {
                let mut rows = tx.query(&sql, params(vec![id.into()])).await?;
                rows.next().await?.map(|row| row_to_job(&row)).transpose()?
            }
            .ok_or_else(|| anyhow::anyhow!("job '{id}' vanished during dispatch claim"))?;
            tx.commit().await?;
            Ok(Some(claimed))
        })
    }

    fn update_job(&self, id: &str, attempt_id: Option<&str>, patch: JobPatch) -> Result<Job> {
        self.guard_writable()?;
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        if let Some(a) = attempt_id {
            if !attempt_id_valid(a) {
                anyhow::bail!("invalid attempt id.");
            }
        }
        validate_job_patch(&patch)?;
        let ts = now();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let row = {
                let mut it = tx
                    .query(
                        "SELECT state, attempt_id, progress_events_json, phase \
                         FROM jobs WHERE id = ?1",
                        params(vec![id.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some((
                        r.get::<String>(0)?,
                        r.get::<Option<String>>(1)?,
                        r.get::<String>(2)?,
                        r.get::<Option<String>>(3)?,
                    )),
                    None => None,
                }
            };
            let (state_str, cur_attempt, events_json, cur_phase) =
                row.ok_or_else(|| anyhow::anyhow!("job '{id}' not found."))?;
            // attempt_id FENCING: a claimed job requires the matching token.
            if let Some(ref claimed) = cur_attempt {
                match attempt_id {
                    Some(a) if a == claimed => {}
                    _ => anyhow::bail!("stale_attempt"),
                }
            }
            let cur_state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
            let new_state = patch.state.unwrap_or(cur_state);
            if patch.state.is_some() && !cur_state.can_transition(new_state) {
                anyhow::bail!(
                    "illegal transition {}->{} for job '{id}'.",
                    cur_state.as_str(),
                    new_state.as_str()
                );
            }
            let events = append_progress_event(
                &events_json,
                ts,
                patch.progress_note.as_deref(),
                new_state,
                patch.phase.as_deref().or(cur_phase.as_deref()),
            );
            let completed_ts: Option<i64> = if new_state.is_terminal() {
                Some(ts)
            } else {
                None
            };
            tx.execute(
                "UPDATE jobs SET \
                    state = ?1, \
                    state_reason = COALESCE(?2, state_reason), \
                    phase = COALESCE(?3, phase), \
                    progress_note = COALESCE(?4, progress_note), \
                    progress_events_json = ?5, \
                    result_summary = COALESCE(?6, result_summary), \
                    result_json = COALESCE(?7, result_json), \
                    error_json = COALESCE(?8, error_json), \
                    artifacts_json = COALESCE(?9, artifacts_json), \
                    completed_ts = COALESCE(?10, completed_ts), \
                    updated_ts = ?11 \
                 WHERE id = ?12",
                params(vec![
                    new_state.as_str().into(),
                    patch.state_reason.clone().into(),
                    patch.phase.clone().into(),
                    patch.progress_note.clone().into(),
                    events.into(),
                    patch.result_summary.clone().into(),
                    patch.result_json.clone().into(),
                    patch.error_json.clone().into(),
                    patch.artifacts_json.clone().into(),
                    completed_ts.into(),
                    ts.into(),
                    id.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.get_job(id)?
            .ok_or_else(|| anyhow::anyhow!("job '{id}' vanished after update."))
    }

    fn job_result(&self, id: &str) -> Result<Option<JobResultView>> {
        match self.get_job(id)? {
            Some(j) => Ok(Some(job_result_view(&j))),
            None => Ok(None),
        }
    }

    fn cancel_job(
        &self,
        id: &str,
        requested_by: &str,
        reason: Option<&str>,
    ) -> Result<Option<Job>> {
        self.guard_writable()?;
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        check_ident("requested_by", requested_by)?;
        if let Some(r) = reason {
            check_job_text("reason", r)?;
        }
        let ts = now();
        let did = self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let state_str = {
                let mut it = tx
                    .query(
                        "SELECT state FROM jobs WHERE id = ?1",
                        params(vec![id.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some(r.get::<String>(0)?),
                    None => None,
                }
            };
            let Some(state_str) = state_str else {
                tx.commit().await?;
                return Ok::<bool, anyhow::Error>(false);
            };
            let state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
            if state == JobState::Queued {
                tx.execute(
                    "UPDATE jobs SET state = ?1, cancel_requested = 1, \
                        cancel_requested_by = COALESCE(cancel_requested_by, ?2), \
                        cancel_requested_ts = COALESCE(cancel_requested_ts, ?3), \
                        cancel_reason = COALESCE(cancel_reason, ?4), \
                        completed_ts = ?3, updated_ts = ?3 \
                     WHERE id = ?5",
                    params(vec![
                        JobState::Cancelled.as_str().into(),
                        requested_by.into(),
                        ts.into(),
                        reason.into(),
                        id.into(),
                    ]),
                )
                .await?;
            } else {
                tx.execute(
                    "UPDATE jobs SET cancel_requested = 1, \
                        cancel_requested_by = COALESCE(cancel_requested_by, ?1), \
                        cancel_requested_ts = COALESCE(cancel_requested_ts, ?2), \
                        cancel_reason = COALESCE(cancel_reason, ?3), \
                        updated_ts = ?2 \
                     WHERE id = ?4",
                    params(vec![
                        requested_by.into(),
                        ts.into(),
                        reason.into(),
                        id.into(),
                    ]),
                )
                .await?;
            }
            tx.commit().await?;
            Ok(true)
        })?;
        if did {
            self.get_job(id)
        } else {
            Ok(None)
        }
    }

    fn record_delivery(
        &self,
        ref_id: i64,
        ref_kind: &str,
        to_peer: &str,
        stage: &str,
        outcome: &str,
    ) -> Result<()> {
        // OWNER-ONLY-WRITES: trap on a read_only handle FIRST (write-trap parity).
        self.guard_writable()?;
        // SECRET-FREE: only these six metadata fields are bound — never a body,
        // subject, sig, or token. The store NEVER injects; it records the outcome its
        // caller computed post-inject. All values bound via params().
        let ts = now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO delivery_log (ref_id, ref_kind, to_peer, stage, outcome, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params(vec![
                        ref_id.into(),
                        ref_kind.into(),
                        to_peer.into(),
                        stage.into(),
                        outcome.into(),
                        ts.into(),
                    ]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })?;
        Ok(())
    }

    fn list_delivery(&self, ref_id: i64, limit: i64) -> Result<Vec<DeliveryTrace>> {
        // BOUNDED: never return more than MAX_DELIVERY_ROWS regardless of `limit`.
        let lim = limit.clamp(1, MAX_DELIVERY_ROWS);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, ref_id, ref_kind, to_peer, stage, outcome, ts
                     FROM delivery_log WHERE ref_id = ?1
                     ORDER BY ts ASC, id ASC LIMIT ?2",
                    params(vec![ref_id.into(), lim.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(DeliveryTrace {
                    id: r.get::<i64>(0)?,
                    ref_id: r.get::<i64>(1)?,
                    ref_kind: r.get::<String>(2)?,
                    to_peer: r.get::<String>(3)?,
                    stage: r.get::<String>(4)?,
                    outcome: r.get::<String>(5)?,
                    ts: r.get::<i64>(6)?,
                });
            }
            Ok(out)
        })
    }

    fn has_delivery(
        &self,
        ref_id: i64,
        ref_kind: &str,
        to_peer: &str,
        stage: &str,
        outcome: &str,
    ) -> Result<bool> {
        if ref_id <= 0 {
            return Ok(false);
        }
        self.block_on_bounded(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT EXISTS(
                        SELECT 1 FROM delivery_log
                        WHERE ref_id = ?1 AND ref_kind = ?2 AND to_peer = ?3
                          AND stage = ?4 AND outcome = ?5
                    )",
                    params(vec![
                        ref_id.into(),
                        ref_kind.into(),
                        to_peer.into(),
                        stage.into(),
                        outcome.into(),
                    ]),
                )
                .await?;
            Ok(rows
                .next()
                .await?
                .map(|row| row.get::<i64>(0))
                .transpose()?
                .unwrap_or(0)
                != 0)
        })
    }

    fn claim_bridge_runtime(
        &self,
        platform: BridgePlatform,
        identity: &str,
        recipient: &str,
        owner_id: &str,
        owner_pid: Option<i64>,
        owner_host: &str,
        stale_before: i64,
    ) -> Result<Option<BridgeRuntimeState>> {
        self.guard_writable()?;
        validate_bridge_claim(
            identity,
            recipient,
            owner_id,
            owner_pid,
            owner_host,
            stale_before,
        )?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let existing: Option<(String, i64)> = {
                let mut rows = tx
                    .query(
                        "SELECT owner_id, heartbeat_ts FROM bridge_runtime WHERE platform = ?1",
                        params(vec![platform.as_str().into()]),
                    )
                    .await?;
                let existing = rows
                    .next()
                    .await?
                    .map(|row| -> Result<(String, i64)> {
                        Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
                    })
                    .transpose()?;
                drop(rows);
                existing
            };
            let may_claim = match &existing {
                None => true,
                Some((current_owner, heartbeat)) => {
                    current_owner.is_empty()
                        || current_owner == owner_id
                        || *heartbeat < stale_before
                }
            };
            if !may_claim {
                tx.commit().await?;
                return Ok(None);
            }
            let heartbeat = now();
            let claim_params = || {
                params(vec![
                    platform.as_str().into(),
                    identity.into(),
                    recipient.into(),
                    owner_id.into(),
                    owner_pid.into(),
                    owner_host.into(),
                    heartbeat.into(),
                ])
            };
            if existing.is_none() {
                tx.execute(
                    "INSERT INTO bridge_runtime (
                        platform, identity, recipient, owner_id, owner_pid, owner_host,
                        heartbeat_ts, status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'starting')",
                    claim_params(),
                )
                .await?;
            } else {
                tx.execute(
                    "UPDATE bridge_runtime SET
                        identity = ?2, recipient = ?3, owner_id = ?4, owner_pid = ?5,
                        owner_host = ?6, heartbeat_ts = ?7, status = 'starting',
                        last_error_class = '', last_error = ''
                     WHERE platform = ?1",
                    claim_params(),
                )
                .await?;
            }
            let state = {
                let sql =
                    format!("SELECT {BRIDGE_RUNTIME_COLS} FROM bridge_runtime WHERE platform = ?1");
                let mut rows = tx
                    .query(&sql, params(vec![platform.as_str().into()]))
                    .await?;
                let row = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("claimed bridge runtime row disappeared"))?;
                let state = row_to_bridge_runtime(&row)?;
                drop(rows);
                state
            };
            tx.commit().await?;
            Ok(Some(state))
        })
    }

    fn update_bridge_runtime(
        &self,
        platform: BridgePlatform,
        owner_id: &str,
        update: &BridgeRuntimeUpdate,
    ) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        validate_bridge_update(update)?;
        let (error_mode, error_class, error_message): (i64, Option<&str>, Option<&str>) =
            match &update.error {
                BridgeRuntimeErrorUpdate::Keep => (0, None, None),
                BridgeRuntimeErrorUpdate::Clear => (1, None, None),
                BridgeRuntimeErrorUpdate::Set { class, message } => {
                    (2, Some(class.as_str()), Some(message.as_str()))
                }
            };
        let heartbeat = now();
        self.rt.block_on(async {
            let changed = self
                .conn
                .execute(
                    "UPDATE bridge_runtime SET
                        cursor = COALESCE(?3, cursor),
                        status = COALESCE(?4, status),
                        last_poll_ts = CASE
                            WHEN ?5 IS NULL OR ?5 <= last_poll_ts THEN last_poll_ts ELSE ?5 END,
                        last_success_ts = CASE
                            WHEN ?6 IS NULL OR ?6 <= last_success_ts THEN last_success_ts ELSE ?6 END,
                        last_delivery_ts = CASE
                            WHEN ?7 IS NULL OR ?7 <= last_delivery_ts THEN last_delivery_ts ELSE ?7 END,
                        last_error_class = CASE ?8
                            WHEN 1 THEN '' WHEN 2 THEN ?9 ELSE last_error_class END,
                        last_error = CASE ?8
                            WHEN 1 THEN '' WHEN 2 THEN ?10 ELSE last_error END,
                        heartbeat_ts = ?11
                     WHERE platform = ?1 AND owner_id = ?2",
                    params(vec![
                        platform.as_str().into(),
                        owner_id.into(),
                        update.cursor.as_deref().into(),
                        update.status.map(BridgeRuntimeStatus::as_str).into(),
                        update.last_poll_ts.into(),
                        update.last_success_ts.into(),
                        update.last_delivery_ts.into(),
                        error_mode.into(),
                        error_class.into(),
                        error_message.into(),
                        heartbeat.into(),
                    ]),
                )
                .await?;
            Ok(changed == 1)
        })
    }

    fn complete_bridge_inbox_snapshot(
        &self,
        platform: BridgePlatform,
        owner_id: &str,
        reader: &str,
        message_ids: &[i64],
        update: &BridgeRuntimeUpdate,
    ) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        validate_bridge_inbox_completion(reader, message_ids, update)?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let owns = {
                let mut rows = tx
                    .query(
                        "SELECT EXISTS(
                            SELECT 1 FROM bridge_runtime
                             WHERE platform = ?1 AND owner_id = ?2
                        )",
                        params(vec![platform.as_str().into(), owner_id.into()]),
                    )
                    .await?;
                let owns = rows
                    .next()
                    .await?
                    .map(|row| row.get::<i64>(0))
                    .transpose()?
                    .unwrap_or(0)
                    != 0;
                drop(rows);
                owns
            };
            if !owns {
                tx.commit().await?;
                return Ok(false);
            }

            let cutoff = now();
            let eligible_sql = format!(
                "SELECT EXISTS(
                    SELECT 1 FROM messages
                     WHERE id = ?2
                       AND (recipient = ?1 OR recipient IN {bc})
                       AND sender != ?1
                       AND superseded_by IS NULL
                       AND (expires_at IS NULL OR expires_at > ?3)
                )",
                bc = BROADCAST_SQL
            );
            let insert_sql = format!(
                "INSERT OR IGNORE INTO reads (message_id, reader, ts)
                 SELECT id, ?1, ?3 FROM messages
                  WHERE id = ?2
                    AND (recipient = ?1 OR recipient IN {bc})
                    AND sender != ?1
                    AND superseded_by IS NULL
                    AND (expires_at IS NULL OR expires_at > ?3)",
                bc = BROADCAST_SQL
            );
            for message_id in message_ids {
                let eligible = {
                    let mut rows = tx
                        .query(
                            &eligible_sql,
                            params(vec![reader.into(), (*message_id).into(), cutoff.into()]),
                        )
                        .await?;
                    let eligible = rows
                        .next()
                        .await?
                        .map(|row| row.get::<i64>(0))
                        .transpose()?
                        .unwrap_or(0)
                        != 0;
                    drop(rows);
                    eligible
                };
                if !eligible {
                    anyhow::bail!(
                        "bridge inbox snapshot row #{message_id} is no longer eligible for its reader."
                    );
                }
                tx.execute(
                    &insert_sql,
                    params(vec![reader.into(), (*message_id).into(), cutoff.into()]),
                )
                .await?;
            }
            if update_bridge_runtime_tx_libsql(&tx, platform, owner_id, update).await? != 1 {
                anyhow::bail!("bridge runtime ownership changed during inbox completion.");
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    fn prepare_bridge_staging(
        &self,
        platform: BridgePlatform,
        owner_id: &str,
        external_identity: &str,
        external_scope: &str,
    ) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        validate_bridge_staging(external_identity, external_scope, &[])?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let owns = {
                let mut rows = tx
                    .query(
                        "SELECT EXISTS(
                            SELECT 1 FROM bridge_runtime
                             WHERE platform = ?1 AND owner_id = ?2
                        )",
                        params(vec![platform.as_str().into(), owner_id.into()]),
                    )
                    .await?;
                let owns = rows
                    .next()
                    .await?
                    .map(|row| row.get::<i64>(0))
                    .transpose()?
                    .unwrap_or(0)
                    != 0;
                drop(rows);
                owns
            };
            if !owns {
                tx.commit().await?;
                return Ok(false);
            }
            tx.execute(
                "DELETE FROM bridge_staged_events
                  WHERE platform = ?1
                    AND (external_identity != ?2 OR external_scope != ?3)",
                params(vec![
                    platform.as_str().into(),
                    external_identity.into(),
                    external_scope.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    fn stage_bridge_events(
        &self,
        platform: BridgePlatform,
        owner_id: &str,
        external_identity: &str,
        external_scope: &str,
        events: &[BridgeStagedEvent],
        update: &BridgeRuntimeUpdate,
    ) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        validate_bridge_staging(external_identity, external_scope, events)?;
        validate_bridge_update(update)?;
        if update.cursor.is_none() {
            anyhow::bail!("staging bridge events requires a cursor update.");
        }
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let owns = {
                let mut rows = tx
                    .query(
                        "SELECT EXISTS(
                            SELECT 1 FROM bridge_runtime
                             WHERE platform = ?1 AND owner_id = ?2
                        )",
                        params(vec![platform.as_str().into(), owner_id.into()]),
                    )
                    .await?;
                let owns = rows
                    .next()
                    .await?
                    .map(|row| row.get::<i64>(0))
                    .transpose()?
                    .unwrap_or(0)
                    != 0;
                drop(rows);
                owns
            };
            if !owns {
                tx.commit().await?;
                return Ok(false);
            }
            for event in events {
                tx.execute(
                    "INSERT INTO bridge_staged_events (
                        platform, external_identity, external_scope, position,
                        order_key, sender, text
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(platform, external_identity, external_scope, position)
                     DO NOTHING",
                    params(vec![
                        platform.as_str().into(),
                        external_identity.into(),
                        external_scope.into(),
                        event.position.as_str().into(),
                        event.order_key.as_str().into(),
                        event.sender.as_deref().into(),
                        event.text.as_deref().into(),
                    ]),
                )
                .await?;
            }
            let (row_count, text_bytes) = {
                let mut rows = tx
                    .query(
                        "SELECT COUNT(*),
                                COALESCE(SUM(length(CAST(COALESCE(text, '') AS BLOB))), 0)
                           FROM bridge_staged_events
                          WHERE platform = ?1
                            AND external_identity = ?2 AND external_scope = ?3",
                        params(vec![
                            platform.as_str().into(),
                            external_identity.into(),
                            external_scope.into(),
                        ]),
                    )
                    .await?;
                let row = rows.next().await?.ok_or_else(|| {
                    anyhow::anyhow!("bridge staging bounds query returned no row")
                })?;
                let counts = (row.get::<i64>(0)?, row.get::<i64>(1)?);
                drop(rows);
                counts
            };
            if row_count > crate::model::MAX_BRIDGE_STAGED_EVENTS
                || text_bytes > crate::model::MAX_BRIDGE_STAGED_TOTAL_BYTES
            {
                anyhow::bail!("bridge staged-event backlog exceeded its durable bound.");
            }
            if update_bridge_runtime_tx_libsql(&tx, platform, owner_id, update).await? != 1 {
                anyhow::bail!("bridge runtime ownership changed during staged page commit.");
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    fn peek_bridge_staged_event(
        &self,
        platform: BridgePlatform,
        external_identity: &str,
        external_scope: &str,
    ) -> Result<Option<BridgeStagedEvent>> {
        validate_bridge_staging(external_identity, external_scope, &[])?;
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT position, order_key, sender, text
                       FROM bridge_staged_events
                      WHERE platform = ?1
                        AND external_identity = ?2 AND external_scope = ?3
                      ORDER BY order_key ASC, position ASC
                      LIMIT 1",
                    params(vec![
                        platform.as_str().into(),
                        external_identity.into(),
                        external_scope.into(),
                    ]),
                )
                .await?;
            let event = rows
                .next()
                .await?
                .map(|row| -> Result<BridgeStagedEvent> {
                    Ok(BridgeStagedEvent {
                        position: row.get::<String>(0)?,
                        order_key: row.get::<String>(1)?,
                        sender: row.get::<Option<String>>(2)?,
                        text: row.get::<Option<String>>(3)?,
                    })
                })
                .transpose()?;
            drop(rows);
            if let Some(event) = &event {
                event.validate().map_err(anyhow::Error::msg)?;
            }
            Ok(event)
        })
    }

    fn complete_bridge_staged_event(
        &self,
        platform: BridgePlatform,
        owner_id: &str,
        external_identity: &str,
        external_scope: &str,
        position: &str,
        update: &BridgeRuntimeUpdate,
    ) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        validate_bridge_staging(
            external_identity,
            external_scope,
            &[BridgeStagedEvent {
                position: position.to_string(),
                order_key: position.to_string(),
                sender: None,
                text: None,
            }],
        )?;
        validate_bridge_update(update)?;
        if update.cursor.is_none() {
            anyhow::bail!("completing a staged bridge event requires a cursor update.");
        }
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let owns = {
                let mut rows = tx
                    .query(
                        "SELECT EXISTS(
                            SELECT 1 FROM bridge_runtime
                             WHERE platform = ?1 AND owner_id = ?2
                        )",
                        params(vec![platform.as_str().into(), owner_id.into()]),
                    )
                    .await?;
                let owns = rows
                    .next()
                    .await?
                    .map(|row| row.get::<i64>(0))
                    .transpose()?
                    .unwrap_or(0)
                    != 0;
                drop(rows);
                owns
            };
            if !owns {
                tx.commit().await?;
                return Ok(false);
            }
            let removed = tx
                .execute(
                    "DELETE FROM bridge_staged_events
                      WHERE platform = ?1 AND external_identity = ?2
                        AND external_scope = ?3 AND position = ?4",
                    params(vec![
                        platform.as_str().into(),
                        external_identity.into(),
                        external_scope.into(),
                        position.into(),
                    ]),
                )
                .await?;
            if removed != 1 {
                anyhow::bail!("staged bridge event disappeared before completion.");
            }
            if update_bridge_runtime_tx_libsql(&tx, platform, owner_id, update).await? != 1 {
                anyhow::bail!("bridge runtime ownership changed during staged event completion.");
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    fn release_bridge_runtime(&self, platform: BridgePlatform, owner_id: &str) -> Result<bool> {
        self.guard_writable()?;
        validate_bridge_owner_id(owner_id)?;
        let heartbeat = now();
        self.rt.block_on(async {
            let changed = self
                .conn
                .execute(
                    "UPDATE bridge_runtime SET
                        owner_id = '', owner_pid = NULL, owner_host = '',
                        heartbeat_ts = ?3, status = 'stopped'
                     WHERE platform = ?1 AND owner_id = ?2",
                    params(vec![
                        platform.as_str().into(),
                        owner_id.into(),
                        heartbeat.into(),
                    ]),
                )
                .await?;
            Ok(changed == 1)
        })
    }

    fn bridge_runtime_status(
        &self,
        platform: BridgePlatform,
    ) -> Result<Option<BridgeRuntimeState>> {
        self.rt.block_on(async {
            let sql =
                format!("SELECT {BRIDGE_RUNTIME_COLS} FROM bridge_runtime WHERE platform = ?1");
            let mut rows = self
                .conn
                .query(&sql, params(vec![platform.as_str().into()]))
                .await?;
            rows.next()
                .await?
                .map(|row| row_to_bridge_runtime(&row))
                .transpose()
        })
    }

    fn list_bridge_runtime_statuses(&self) -> Result<Vec<BridgeRuntimeState>> {
        self.rt.block_on(async {
            let sql = format!(
                "SELECT {BRIDGE_RUNTIME_COLS} FROM bridge_runtime
                 ORDER BY CASE platform WHEN 'telegram' THEN 0 WHEN 'slack' THEN 1 ELSE 2 END"
            );
            let mut rows = self.conn.query(&sql, ()).await?;
            let mut states = Vec::new();
            while let Some(row) = rows.next().await? {
                states.push(row_to_bridge_runtime(&row)?);
            }
            Ok(states)
        })
    }

    fn heartbeat(&self, name: &str, host: &str, pid: Option<i64>) -> Result<()> {
        check_ident("peer name", name)?;
        let ts = crate::model::now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO presence (name, host, pid, heartbeat_ts)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(name) DO UPDATE SET
                         host = excluded.host,
                         pid = excluded.pid,
                         heartbeat_ts = excluded.heartbeat_ts",
                    params(vec![name.into(), host.into(), pid.into(), ts.into()]),
                )
                .await
                .context("heartbeat")?;
            Ok(())
        })
    }

    fn presence(&self, name: &str, host: &str) -> Result<Option<i64>> {
        let cutoff = crate::model::now().saturating_sub(PRESENCE_TTL_SECS);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT heartbeat_ts FROM presence
                     WHERE name = ?1 AND host = ?2 AND heartbeat_ts >= ?3
                     LIMIT 1",
                    params(vec![name.into(), host.into(), cutoff.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(r.get::<i64>(0)?)),
                None => Ok(None),
            }
        })
    }

    fn evict_stale_presence(&self, cutoff_secs: i64) -> Result<usize> {
        let cutoff = crate::model::now().saturating_sub(cutoff_secs);
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "DELETE FROM presence WHERE heartbeat_ts < ?1",
                    params(vec![cutoff.into()]),
                )
                .await
                .context("evict_stale_presence")?;
            Ok(n as usize)
        })
    }

    // ── WL-016 scheduler ──────────────────────────────────────────────────────
    #[allow(clippy::too_many_arguments)]
    fn schedule_message(
        &self,
        sender: &str,
        recipient: &str,
        subject: Option<&str>,
        body: &str,
        kind: ScheduleKind,
        cron_expr: &str,
        next_run: i64,
    ) -> Result<i64> {
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_ident("recipient", recipient)?;
        crate::store::check_subject(subject)?;
        check_body(body)?;
        if cron_expr.len() > MAX_CRON_EXPR_LEN {
            anyhow::bail!(
                "cron expression is too long ({} chars; max {MAX_CRON_EXPR_LEN}).",
                cron_expr.len()
            );
        }
        let ts = now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO schedules (kind, cron_expr, next_run, sender, recipient, subject, body, created_ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params(vec![
                        kind.as_str().into(),
                        cron_expr.into(),
                        next_run.into(),
                        sender.into(),
                        recipient.into(),
                        subject.map(|s| s.to_string()).into(),
                        body.into(),
                        ts.into(),
                    ]),
                )
                .await?;
            Ok(self.conn.last_insert_rowid())
        })
    }

    fn list_schedules(&self, sender: &str, limit: i64) -> Result<Vec<Schedule>> {
        check_ident("sender", sender)?;
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, kind, cron_expr, next_run, sender, recipient, subject, body, created_ts, executed_ts, cancelled
                     FROM schedules WHERE sender = ?1 ORDER BY created_ts DESC LIMIT ?2",
                    params(vec![sender.into(), limit.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_schedule(&r)?);
            }
            Ok(out)
        })
    }

    fn cancel_schedule(&self, id: i64) -> Result<bool> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "UPDATE schedules SET cancelled = 1 WHERE id = ?1 AND cancelled = 0 AND executed_ts IS NULL",
                    params(vec![id.into()]),
                )
                .await?;
            Ok(n > 0)
        })
    }

    fn get_due_schedules(&self, before_ts: i64) -> Result<Vec<Schedule>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, kind, cron_expr, next_run, sender, recipient, subject, body, created_ts, executed_ts, cancelled
                     FROM schedules
                     WHERE next_run <= ?1 AND cancelled = 0
                       AND (executed_ts IS NULL OR kind = 'recurring')
                     ORDER BY next_run ASC",
                    params(vec![before_ts.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_schedule(&r)?);
            }
            Ok(out)
        })
    }

    fn mark_schedule_executed(&self, id: i64) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let row: Option<(String, String)> = {
                let mut it = tx
                    .query(
                        "SELECT kind, cron_expr FROM schedules WHERE id = ?1",
                        params(vec![id.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Some((r.get::<String>(0)?, r.get::<String>(1)?)),
                    None => None,
                }
            };
            let Some((kind_str, cron_expr)) = row else {
                tx.commit().await?;
                return Ok(());
            };
            let kind = ScheduleKind::from_str(&kind_str).map_err(|m| anyhow::anyhow!(m))?;
            match kind {
                ScheduleKind::OneShot => {
                    tx.execute(
                        "UPDATE schedules SET executed_ts = ?1 WHERE id = ?2",
                        params(vec![now().into(), id.into()]),
                    )
                    .await?;
                }
                ScheduleKind::Recurring => {
                    let next = crate::model::next_occurrence(&cron_expr, now());
                    if let Some(ts) = next {
                        tx.execute(
                            "UPDATE schedules SET next_run = ?1 WHERE id = ?2",
                            params(vec![ts.into(), id.into()]),
                        )
                        .await?;
                    } else {
                        tx.execute(
                            "UPDATE schedules SET cancelled = 1 WHERE id = ?1",
                            params(vec![id.into()]),
                        )
                        .await?;
                    }
                }
            }
            tx.commit().await?;
            Ok(())
        })
    }

    fn add_review_item(
        &self,
        pr_url: &str,
        title: &str,
        author: &str,
        repo: &str,
        state: ReviewItemState,
        review_requested_at: Option<i64>,
    ) -> Result<String> {
        self.guard_writable()?;
        if !pr_url_valid(pr_url) {
            anyhow::bail!("pr_url must be a valid GitHub pull request URL");
        }
        if title.len() > MAX_REVIEW_TITLE_LEN {
            anyhow::bail!("title exceeds {} chars", MAX_REVIEW_TITLE_LEN);
        }
        if author.len() > MAX_REVIEW_IDENT_LEN {
            anyhow::bail!("author exceeds {} chars", MAX_REVIEW_IDENT_LEN);
        }
        if repo.len() > MAX_REVIEW_IDENT_LEN {
            anyhow::bail!("repo exceeds {} chars", MAX_REVIEW_IDENT_LEN);
        }
        let id = new_review_id(now());
        let created_at = now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO reviews (id, pr_url, title, author, repo, state, review_requested_at, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params(vec![
                        id.clone().into(),
                        pr_url.into(),
                        title.into(),
                        author.into(),
                        repo.into(),
                        state.as_str().into(),
                        review_requested_at.into(),
                        created_at.into(),
                    ]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(id)
    }

    fn review_queue(&self, filter: ReviewQueueFilter, limit: i64) -> Result<Vec<ReviewItem>> {
        let limit = clamp_limit(limit);
        let (where_clause, binds): (&str, Vec<Value>) = match filter {
            ReviewQueueFilter::All => ("", Vec::new()),
            ReviewQueueFilter::Open => {
                let p: Vec<Value> = vec!["open".into()];
                ("WHERE state = ?1", p)
            }
            ReviewQueueFilter::Pending => {
                let p: Vec<Value> = vec!["open".into()];
                ("WHERE state = ?1 AND reviewed_at IS NULL", p)
            }
            ReviewQueueFilter::Reviewed => ("WHERE reviewed_at IS NOT NULL", Vec::new()),
        };
        let sql = format!(
            "SELECT id, pr_url, title, author, repo, state, review_requested_at, reviewed_at, reviewed_by, created_at
             FROM reviews {} ORDER BY created_at DESC LIMIT {}",
            where_clause, limit
        );
        self.rt.block_on(async {
            let mut it = self.conn.query(&sql, params(binds)).await?;
            let mut items = Vec::new();
            while let Some(r) = it.next().await? {
                items.push(ReviewItem {
                    id: r.get(0)?,
                    pr_url: r.get(1)?,
                    title: r.get(2)?,
                    author: r.get(3)?,
                    repo: r.get(4)?,
                    state: ReviewItemState::from_str(&r.get::<String>(5)?)
                        .map_err(|e| anyhow::anyhow!(e))?,
                    review_requested_at: r.get(6)?,
                    reviewed_at: r.get(7)?,
                    reviewed_by: r.get(8)?,
                    created_at: r.get(9)?,
                });
            }
            Ok::<_, anyhow::Error>(items)
        })
    }

    fn mark_reviewed(&self, id: &str, reviewer: &str) -> Result<bool> {
        self.guard_writable()?;
        check_ident("reviewer", reviewer)?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "UPDATE reviews SET reviewed_at = ?1, reviewed_by = ?2 WHERE id = ?3",
                    params(vec![now().into(), reviewer.into(), id.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(n > 0)
        })
    }

    fn remove_review_item(&self, id: &str) -> Result<bool> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute("DELETE FROM reviews WHERE id = ?1", params(vec![id.into()]))
                .await?;
            Ok::<_, anyhow::Error>(n > 0)
        })
    }

    fn reserve_lease(
        &self,
        holder: &str,
        resource: &str,
        ttl_secs: i64,
        note: Option<&str>,
    ) -> Result<Lease> {
        use crate::model::{
            lease_path_conflicts, lease_path_normalize, lease_resource_valid, lease_ttl_valid,
        };
        if !lease_resource_valid(resource) {
            anyhow::bail!("invalid resource string");
        }
        if !lease_ttl_valid(ttl_secs) {
            anyhow::bail!(
                "ttl must be > 0 and <= {}s",
                crate::model::MAX_LEASE_TTL_SECS
            );
        }
        let note = note.unwrap_or("");
        if note.len() > crate::model::MAX_LEASE_NOTE_LEN {
            anyhow::bail!("note exceeds {} chars", crate::model::MAX_LEASE_NOTE_LEN);
        }
        let resource_norm = lease_path_normalize(resource);
        if resource_norm.is_empty() {
            anyhow::bail!("invalid resource path");
        }
        let acquired = now();
        let expires = acquired + ttl_secs;

        self.guard_writable()?;
        self.rt.block_on(async {
            // Sweep expired leases inline (cannot call sweep_expired_leases
            // because it also uses block_on).
            self.conn
                .execute(
                    "DELETE FROM leases WHERE expires <= ?1",
                    params(vec![now().into()]),
                )
                .await?;

            // Check for path conflicts (exact, parent, child) with any *other* holder.
            let mut stmt = self
                .conn
                .query(
                    "SELECT resource, holder, expires FROM leases
                     WHERE expires > ?1
                       AND (resource = ?2
                            OR resource || '/' = SUBSTR(?2, 1, LENGTH(resource) + 1)
                            OR ?2 || '/' = SUBSTR(resource, 1, LENGTH(?2) + 1))",
                    params(vec![now().into(), resource_norm.clone().into()]),
                )
                .await?;
            let mut conflicts: Vec<(String, String, i64)> = Vec::new();
            while let Some(row) = stmt.next().await? {
                conflicts.push((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<i64>(2)?,
                ));
            }
            for (existing_res, existing_holder, existing_expires) in conflicts {
                if existing_holder == holder && existing_res == resource_norm {
                    self.conn
                        .execute(
                            "UPDATE leases SET acquired = ?1, expires = ?2, note = ?3
                             WHERE resource = ?4 AND holder = ?5",
                            params(vec![
                                acquired.into(),
                                expires.into(),
                                note.into(),
                                resource_norm.clone().into(),
                                holder.into(),
                            ]),
                        )
                        .await?;
                    return Ok::<_, anyhow::Error>(Lease {
                        resource: resource_norm,
                        holder: holder.to_string(),
                        acquired,
                        expires,
                        note: note.to_string(),
                    });
                }
                if lease_path_conflicts(&existing_res, &resource_norm) {
                    anyhow::bail!(
                        "resource '{}' conflicts with '{}' held by '{}' until {}",
                        resource,
                        existing_res,
                        existing_holder,
                        existing_expires
                    );
                }
            }

            let n = self
                .conn
                .execute(
                    "INSERT INTO leases (resource, holder, acquired, expires, note)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(resource) DO UPDATE SET
                         holder = excluded.holder,
                         acquired = excluded.acquired,
                         expires = excluded.expires,
                         note = excluded.note
                     WHERE leases.expires < ?6",
                    params(vec![
                        resource_norm.into(),
                        holder.into(),
                        acquired.into(),
                        expires.into(),
                        note.into(),
                        now().into(),
                    ]),
                )
                .await?;
            if n == 0 {
                anyhow::bail!("resource '{}' is already held", resource);
            }

            Ok::<_, anyhow::Error>(Lease {
                resource: resource.to_string(),
                holder: holder.to_string(),
                acquired,
                expires,
                note: note.to_string(),
            })
        })
    }

    fn release_lease(&self, holder: &str, resource: &str) -> Result<bool> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "DELETE FROM leases WHERE resource = ?1 AND holder = ?2",
                    params(vec![resource.into(), holder.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(n > 0)
        })
    }

    fn list_leases(&self, limit: i64) -> Result<Vec<Lease>> {
        self.rt.block_on(async {
            // Sweep expired leases inline.
            self.conn
                .execute(
                    "DELETE FROM leases WHERE expires <= ?1",
                    params(vec![now().into()]),
                )
                .await?;
            let now = now();
            let limit = clamp_limit(limit);
            let mut stmt = self
                .conn
                .query(
                    "SELECT resource, holder, acquired, expires, note
                     FROM leases WHERE expires > ?1
                     ORDER BY acquired DESC LIMIT ?2",
                    params(vec![now.into(), limit.into()]),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(row) = stmt.next().await? {
                out.push(Lease {
                    resource: row.get(0)?,
                    holder: row.get(1)?,
                    acquired: row.get(2)?,
                    expires: row.get(3)?,
                    note: row.get(4)?,
                });
            }
            Ok::<_, anyhow::Error>(out)
        })
    }

    fn sweep_expired_leases(&self) -> Result<usize> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let n = self
                .conn
                .execute(
                    "DELETE FROM leases WHERE expires <= ?1",
                    params(vec![now().into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(n as usize)
        })
    }

    fn set_message_priority(&self, id: i64, priority: &str) -> Result<()> {
        let p = crate::model::MessagePriority::parse(priority);
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE messages SET priority = ?1 WHERE id = ?2",
                    params(vec![p.as_str().into(), id.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn set_message_expiry(&self, id: i64, expires_at: i64) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE messages SET expires_at = ?1 WHERE id = ?2",
                    params(vec![expires_at.into(), id.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn sweep_expired_messages(&self) -> Result<usize> {
        self.guard_writable()?;
        let now = now();
        self.rt.block_on(async {
            // Delete-on-sweep: reads first, then messages, in one IMMEDIATE tx
            // (the SqliteStore mirror).
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            tx.execute(
                "UPDATE asks SET reply_to = NULL
                  WHERE reply_to IN (
                        SELECT id FROM asks
                         WHERE question_msg_id IN (
                                   SELECT id FROM messages
                                    WHERE expires_at IS NOT NULL AND expires_at <= ?1)
                            OR answer_msg_id IN (
                                   SELECT id FROM messages
                                    WHERE expires_at IS NOT NULL AND expires_at <= ?1)
                  )",
                params(vec![now.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM asks
                  WHERE question_msg_id IN (
                            SELECT id FROM messages
                             WHERE expires_at IS NOT NULL AND expires_at <= ?1)
                     OR answer_msg_id IN (
                            SELECT id FROM messages
                             WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
                params(vec![now.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM ask_groups
                  WHERE NOT EXISTS (SELECT 1 FROM asks WHERE asks.parent_id = ask_groups.parent_id)",
                (),
            )
            .await?;
            tx.execute(
                "UPDATE messages SET in_reply_to = NULL
                  WHERE in_reply_to IN
                        (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
                params(vec![now.into()]),
            )
            .await?;
            tx.execute(
                "UPDATE messages SET superseded_by = NULL
                  WHERE superseded_by IN
                        (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
                params(vec![now.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM reads WHERE message_id IN
                    (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
                params(vec![now.into()]),
            )
            .await?;
            let n = tx
                .execute(
                    "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                    params(vec![now.into()]),
                )
                .await?;
            if n > 0 {
                tx.execute("DELETE FROM summaries", ()).await?;
            }
            tx.commit().await?;
            Ok::<_, anyhow::Error>(n as usize)
        })
    }

    fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()> {
        self.guard_writable()?;
        if new_id <= old_id {
            anyhow::bail!("cannot supersede: successor id must be newer than predecessor id");
        }
        self.rt.block_on(async {
            // Validate both routes and write the link under one writer lock so a
            // concurrent expiry/retention sweep cannot remove the successor in
            // the validation-to-update window.
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let mut it = tx
                .query(
                    "SELECT sender, recipient FROM messages WHERE id = ?1",
                    params(vec![old_id.into()]),
                )
                .await?;
            let (old_sender, old_recipient) = match it.next().await? {
                Some(r) => (r.get::<String>(0)?, r.get::<String>(1)?),
                None => anyhow::bail!("cannot supersede: message #{old_id} does not exist"),
            };
            drop(it);
            let mut it = tx
                .query(
                    "SELECT sender, recipient FROM messages WHERE id = ?1",
                    params(vec![new_id.into()]),
                )
                .await?;
            let (new_sender, new_recipient) = match it.next().await? {
                Some(r) => (r.get::<String>(0)?, r.get::<String>(1)?),
                None => {
                    anyhow::bail!("cannot supersede: successor message #{new_id} does not exist")
                }
            };
            drop(it);
            // Authorization: only the ORIGINAL SENDER of old_id may supersede it
            // (best-effort same-identity guard; censorship/DoS protection).
            if old_sender != caller {
                anyhow::bail!(
                    "cannot supersede: #{old_id} was sent by '{old_sender}', not '{caller}'"
                );
            }
            if new_sender != caller || new_recipient != old_recipient {
                anyhow::bail!("cannot supersede: successor must use the same sender and recipient");
            }
            tx.execute(
                "UPDATE messages SET superseded_by = ?2 WHERE id = ?1",
                params(vec![old_id.into(), new_id.into()]),
            )
            .await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn supersede_prior_idle(&self, sender: &str, recipient: &str, new_id: i64) -> Result<usize> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let mut routes = tx
                .query(
                    "SELECT sender, recipient FROM messages WHERE id = ?1",
                    params(vec![new_id.into()]),
                )
                .await?;
            let (new_sender, new_recipient) = match routes.next().await? {
                Some(row) => (row.get::<String>(0)?, row.get::<String>(1)?),
                None => anyhow::bail!(
                    "cannot supersede idle messages: successor #{new_id} does not exist"
                ),
            };
            drop(routes);
            if new_sender != sender || new_recipient != recipient {
                anyhow::bail!(
                    "cannot supersede idle messages: successor must use the same sender and recipient"
                );
            }
            // Stamp the new ping as idle (scoped to `sender` = self-only authz),
            // then auto-supersede the sender's prior UNREAD idle pings to this
            // recipient. The predicate is the SqliteStore mirror — kind='idle'
            // excludes every real message, sender/recipient scope it, id<>new_id
            // makes an idempotency replay a no-op, superseded_by IS NULL skips
            // chained rows, and the NOT EXISTS clause is the SAME unread
            // definition used by the unread count.
            tx.execute(
                "UPDATE messages SET kind = ?3 WHERE id = ?1 AND sender = ?2",
                params(vec![
                    new_id.into(),
                    sender.into(),
                    crate::model::KIND_IDLE.into(),
                ]),
            )
            .await?;
            let n = tx
                .execute(
                    "UPDATE messages SET superseded_by = ?1
                     WHERE sender = ?2 AND recipient = ?3
                       AND kind = ?4
                       AND superseded_by IS NULL
                       AND id <> ?1
                       AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = messages.id AND r.reader = ?3)",
                    params(vec![
                        new_id.into(),
                        sender.into(),
                        recipient.into(),
                        crate::model::KIND_IDLE.into(),
                    ]),
                )
                .await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(n as usize)
        })
    }

    fn set_peer_policy(&self, name: &str, policy: &str) -> Result<()> {
        let p = crate::model::ContactPolicy::parse(policy);
        self.guard_writable()?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE peers SET contact_policy = ?1 WHERE name = ?2",
                    params(vec![p.as_str().into(), name.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn get_peer_policy(&self, name: &str) -> Result<Option<String>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT contact_policy FROM peers WHERE name = ?1",
                    params(vec![name.into()]),
                )
                .await?;
            match it.next().await? {
                Some(row) => Ok::<_, anyhow::Error>(Some(row.get::<String>(0)?)),
                None => Ok(None),
            }
        })
    }

    fn permission_verdict(
        &self,
        correlation_id: &str,
        timeout_secs: i64,
    ) -> Result<(PermissionStatus, Option<String>)> {
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        let ask = self
            .get_ask(correlation_id)?
            .ok_or_else(|| anyhow::anyhow!("no ask found for {correlation_id}"))?;
        if ask.kind != AskKind::ToolPermission {
            anyhow::bail!("ask {correlation_id} is not a tool permission.");
        }
        let answer_body: Option<String> = if let Some(aid) = ask.answer_msg_id {
            self.rt.block_on(async {
                let mut it = self
                    .conn
                    .query(
                        "SELECT body FROM messages WHERE id = ?1",
                        params(vec![aid.into()]),
                    )
                    .await?;
                match it.next().await? {
                    Some(r) => Ok::<_, anyhow::Error>(r.get::<String>(0).ok()),
                    None => Ok(None),
                }
            })?
        } else {
            None
        };
        let timeout = if timeout_secs > 0 {
            timeout_secs
        } else {
            crate::model::PERMISSION_TIMEOUT_SECS
        };
        let status = permission_status(&ask, answer_body.as_deref(), now(), timeout);
        Ok((status, answer_body))
    }

    fn list_permissions(&self, me: &str, limit: i64) -> Result<Vec<Ask>> {
        check_ident("me", me)?;
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
                            options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id
                     FROM asks
                     WHERE asker = ?1 AND kind = ?2
                     ORDER BY opened_ts DESC LIMIT ?3",
                    params(vec![
                        me.into(),
                        AskKind::ToolPermission.as_str().into(),
                        limit.into(),
                    ]),
                )
                .await?;
            let mut asks = Vec::new();
            while let Some(r) = it.next().await? {
                asks.push(row_to_ask(&r)?);
            }
            Ok::<_, anyhow::Error>(asks)
        })
    }

    fn store_summary(&self, root_id: i64, text: &str, model: &str) -> Result<()> {
        self.guard_writable()?;
        let ts = now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO summaries
                         (root_id, text, model, created_ts, refreshed_ts, generation)
                     VALUES (?1, ?2, ?3, ?4, ?5,
                             (SELECT generation FROM summary_state WHERE singleton = 1))
                 ON CONFLICT(root_id) DO UPDATE SET
                     text = excluded.text,
                     model = excluded.model,
                     refreshed_ts = excluded.refreshed_ts,
                     generation = excluded.generation",
                    params(vec![
                        root_id.into(),
                        text.into(),
                        model.into(),
                        ts.into(),
                        ts.into(),
                    ]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn summary_generation(&self) -> Result<i64> {
        self.rt.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT generation FROM summary_state WHERE singleton = 1",
                    (),
                )
                .await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("summary generation state is missing"))?;
            Ok::<_, anyhow::Error>(row.get::<i64>(0)?)
        })
    }

    fn store_summary_if_generation(
        &self,
        root_id: i64,
        text: &str,
        model: &str,
        expected_generation: i64,
    ) -> Result<bool> {
        self.guard_writable()?;
        let ts = now();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let changed = tx
                .execute(
                    "INSERT INTO summaries
                         (root_id, text, model, created_ts, refreshed_ts, generation)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6
                     WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?1)
                       AND EXISTS (
                           SELECT 1 FROM summary_state
                           WHERE singleton = 1 AND generation = ?6
                       )
                     ON CONFLICT(root_id) DO UPDATE SET
                         text = excluded.text,
                         model = excluded.model,
                         refreshed_ts = excluded.refreshed_ts,
                         generation = excluded.generation",
                    params(vec![
                        root_id.into(),
                        text.into(),
                        model.into(),
                        ts.into(),
                        ts.into(),
                        expected_generation.into(),
                    ]),
                )
                .await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(changed > 0)
        })
    }

    fn get_summary(&self, root_id: i64) -> Result<Option<crate::model::Summary>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT s.root_id, s.text, s.model, s.created_ts, s.refreshed_ts
                     FROM summaries s
                     JOIN messages m ON m.id = s.root_id
                     JOIN summary_state state
                       ON state.singleton = 1 AND state.generation = s.generation
                     WHERE s.root_id = ?1",
                    params(vec![root_id.into()]),
                )
                .await?;
            if let Some(r) = it.next().await? {
                Ok(Some(crate::model::Summary {
                    root_id: r.get(0)?,
                    text: r.get(1)?,
                    model: r.get(2)?,
                    created_ts: r.get(3)?,
                    refreshed_ts: r.get(4)?,
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn delete_summary(&self, root_id: i64) -> Result<bool> {
        self.guard_writable()?;
        self.rt.block_on(async {
            let rows = self
                .conn
                .execute(
                    "DELETE FROM summaries WHERE root_id = ?1",
                    params(vec![root_id.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(rows > 0)
        })
    }
}

/// Count unread messages for `me` against any connection (the live connection or
/// an open transaction, which derefs to `Connection`), so the count can share a
/// transaction with the inbox read+mark for a consistent snapshot.
async fn unread_count_on(conn: &Connection, me: &str) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > ?2)
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
        bc = BROADCAST_SQL
    );
    let mut it = conn
        .query(&sql, params(vec![me.into(), now().into()]))
        .await?;
    match it.next().await? {
        Some(r) => Ok(r.get::<i64>(0)?),
        None => Ok(0),
    }
}

async fn peek_oldest_unread_on(conn: &Connection, me: &str) -> Result<Option<Message>> {
    let sql = format!(
        "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind, request_priority, request_ttl, request_supersedes, request_dedup_idle FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > ?2)
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)
         ORDER BY m.id ASC LIMIT 1",
        bc = BROADCAST_SQL
    );
    let mut it = conn
        .query(&sql, params(vec![me.into(), now().into()]))
        .await?;
    match it.next().await? {
        Some(r) => Ok(Some(row_to_message(&r)?)),
        None => Ok(None),
    }
}

async fn wake_last_acked_on(conn: &Connection, me: &str) -> Result<i64> {
    let mut it = conn
        .query(
            "SELECT COALESCE(last_id, 0) FROM wake_acks WHERE reader = ?1",
            params(vec![me.into()]),
        )
        .await?;
    match it.next().await? {
        Some(r) => Ok(r.get::<i64>(0)?),
        None => Ok(0),
    }
}

/// Count unread messages for `me` against an open transaction.
async fn unread_count_tx(tx: &libsql::Transaction, me: &str) -> Result<i64> {
    unread_count_on(tx, me).await
}

impl LibsqlStore {
    /// Async core of `unread_count`, reused by `inbox`/`sessions` without
    /// re-entering the runtime (`block_on` cannot be nested).
    async fn unread_count_async(&self, me: &str) -> Result<i64> {
        unread_count_on(&self.conn, me).await
    }

    /// Async core of `total_messages`, reused by `clear_all`.
    async fn total_messages_async(&self) -> Result<i64> {
        let mut it = self.conn.query("SELECT COUNT(*) FROM messages", ()).await?;
        match it.next().await? {
            Some(r) => Ok(r.get::<i64>(0)?),
            None => Ok(0),
        }
    }

    /// Test-only: backdate a peer's `last_seen` by `secs` so presence and
    /// touch-without-clobber paths can be exercised deterministically without
    /// waiting wall-clock time. Mirrors the raw UPDATE the SqliteStore tests use
    /// against their connection (which is private here, so this lives on the impl).
    #[cfg(test)]
    fn backdate_peer(&self, name: &str, secs: i64) -> Result<()> {
        self.rt.block_on(async {
            self.conn
                .execute(
                    "UPDATE peers SET last_seen = last_seen - ?1 WHERE name = ?2",
                    params(vec![secs.into(), name.into()]),
                )
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        clamp_limit, is_alive, is_online, liveness_for, pid_alive, Liveness, MAX_IDENT, MAX_LIMIT,
        ONLINE_TTL_SECS,
    };

    /// A local-file libsql store backed by a unique temp DB. This exercises the
    /// REAL libsql backend (its own SQLite core, async-over-block_on bridge, and
    /// the 5-arg `register_peer`/`socket` wiring) directly — coverage the
    /// black-box integration suite drives only through the CLI binary.
    fn mem() -> LibsqlStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            db: Some(dir.join("t.db").to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        LibsqlStore::open(&cfg).unwrap()
    }

    #[test]
    fn keyed_ask_preserves_explicit_subject_shape_after_parent_removal_libsql() {
        let s = mem();
        let (root_ask, root_mid) = s
            .ask(
                "a",
                "b",
                Some("Topic"),
                "root question",
                AskKind::FreeText,
                None,
                None,
            )
            .unwrap();
        let (child_ask, child_mid, created) = s
            .ask_idempotent(
                "a",
                "b",
                Some("Re: Topic"),
                "follow-up",
                AskKind::FreeText,
                None,
                Some(&root_ask),
                Some("event:subject-shape"),
            )
            .unwrap();
        assert!(created);
        s.rt.block_on(async {
            s.conn
                .execute(
                    "DELETE FROM messages WHERE id = ?1",
                    params(vec![root_mid.into()]),
                )
                .await
        })
        .unwrap();

        assert_eq!(
            s.ask_idempotent(
                "a",
                "b",
                Some("Re: Topic"),
                "follow-up",
                AskKind::FreeText,
                None,
                Some(&root_ask),
                Some("event:subject-shape"),
            )
            .unwrap(),
            (child_ask, child_mid, false)
        );
        assert!(s
            .ask_idempotent(
                "a",
                "b",
                None,
                "follow-up",
                AskKind::FreeText,
                None,
                Some(&root_ask),
                Some("event:subject-shape"),
            )
            .is_err());
    }

    #[test]
    fn configured_send_is_atomic_and_replays_request_options_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-configured-send-libsql-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };

        let s = LibsqlStore::open(&cfg).unwrap();
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "must roll back",
                Some("event:configured-invalid-predecessor"),
                None,
                Some("urgent"),
                Some(999_999),
                3_600,
                false,
            )
            .is_err());
        assert!(s
            .message_by_idempotency_key("event:configured-invalid-predecessor")
            .unwrap()
            .is_none());
        assert_eq!(s.total_messages().unwrap(), 0);

        let predecessor = s.send("a", "b", Some("v1"), "first", None, None).unwrap();
        let alternate = s
            .send("a", "b", Some("other"), "other", None, None)
            .unwrap();
        let (replacement, created) = s
            .send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                Some("trace:first"),
                Some("urgent"),
                Some(predecessor),
                3_600,
                false,
            )
            .unwrap();
        assert!(created);
        let stored = s
            .message_by_idempotency_key("event:configured-send")
            .unwrap()
            .unwrap();
        assert_eq!(stored.priority, "urgent");
        assert_eq!(
            stored.expires_at,
            Some(crate::model::expiry_from_ttl(stored.ts, 3_600))
        );
        assert_eq!(superseded_by_of(&s, "b", predecessor), Some(replacement));
        drop(s);

        let s = LibsqlStore::open(&cfg).unwrap();
        assert_eq!(
            s.send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                Some("trace:retry"),
                Some("urgent"),
                Some(predecessor),
                3_600,
                false,
            )
            .unwrap(),
            (replacement, false)
        );
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                None,
                Some("high"),
                Some(predecessor),
                3_600,
                false,
            )
            .is_err());
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                None,
                Some("urgent"),
                Some(predecessor),
                1_800,
                false,
            )
            .is_err());
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                None,
                Some("urgent"),
                Some(alternate),
                3_600,
                false,
            )
            .is_err());
        assert_eq!(s.total_messages().unwrap(), 3);
        assert_eq!(superseded_by_of(&s, "b", predecessor), Some(replacement));
        assert_eq!(superseded_by_of(&s, "b", alternate), None);
        s.set_message_expiry(predecessor, now().saturating_sub(1))
            .unwrap();
        assert_eq!(s.sweep_expired_messages().unwrap(), 1);
        assert!(!s.message_exists(predecessor).unwrap());
        assert_eq!(
            s.send_configured_idempotent(
                "a",
                "b",
                Some("v2"),
                "replacement",
                Some("event:configured-send"),
                Some("trace:after-gc"),
                Some("urgent"),
                Some(predecessor),
                3_600,
                false,
            )
            .unwrap(),
            (replacement, false),
            "private request_supersedes metadata survives predecessor GC"
        );
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configured_idle_dedup_replay_preserves_first_mutation_libsql() {
        let s = mem();
        let (first, first_created) = s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting once",
                Some("event:configured-idle-1"),
                None,
                Some("normal"),
                None,
                0,
                true,
            )
            .unwrap();
        assert!(first_created);
        let (second, second_created) = s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting again",
                Some("event:configured-idle-2"),
                Some("trace:first-idle"),
                Some("normal"),
                None,
                0,
                true,
            )
            .unwrap();
        assert!(second_created);
        assert_eq!(superseded_by_of(&s, "b", first), Some(second));
        assert_eq!(superseded_by_of(&s, "b", second), None);
        assert_eq!(
            s.message_by_idempotency_key("event:configured-idle-2")
                .unwrap()
                .unwrap()
                .kind
                .as_deref(),
            Some(crate::model::KIND_IDLE)
        );
        assert_eq!(
            s.send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting again",
                Some("event:configured-idle-2"),
                Some("trace:retry-idle"),
                Some("normal"),
                None,
                0,
                true,
            )
            .unwrap(),
            (second, false)
        );
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting again",
                Some("event:configured-idle-2"),
                None,
                Some("normal"),
                None,
                0,
                false,
            )
            .is_err());
        assert!(s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting again",
                Some("event:configured-idle-2"),
                None,
                Some("normal"),
                None,
                60,
                true,
            )
            .is_err());
        assert_eq!(s.total_messages().unwrap(), 2);
        assert_eq!(superseded_by_of(&s, "b", first), Some(second));
        assert_eq!(superseded_by_of(&s, "b", second), None);
    }

    #[test]
    fn legacy_v1_idle_metadata_replays_configured_dedup_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-legacy-idle-libsql-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Builder::new_local(&path).build().await.unwrap();
                let conn = db.connect().unwrap();
                conn.execute_batch(
                    "CREATE TABLE messages (
                        id              INTEGER PRIMARY KEY AUTOINCREMENT,
                        ts              INTEGER NOT NULL,
                        sender          TEXT NOT NULL,
                        recipient       TEXT NOT NULL,
                        subject         TEXT,
                        body            TEXT NOT NULL,
                        in_reply_to     INTEGER,
                        idempotency_key TEXT,
                        trace_id        TEXT,
                        priority        TEXT NOT NULL DEFAULT 'normal',
                        superseded_by   INTEGER,
                        expires_at      INTEGER,
                        kind            TEXT
                     );
                     INSERT INTO messages
                        (id, ts, sender, recipient, body, idempotency_key, priority,
                         superseded_by, kind)
                     VALUES
                        (1, 1, 'a', 'b', 'waiting once', 'event:legacy-idle-1',
                         'urgent', 2, 'idle'),
                        (2, 2, 'a', 'b', 'waiting again', 'event:legacy-idle-2',
                         'urgent', NULL, 'idle');",
                )
                .await
                .unwrap();
                let mut columns = conn
                    .query(
                        "SELECT name FROM pragma_table_info('messages')
                         WHERE name IN ('request_supersedes', 'request_dedup_idle')",
                        (),
                    )
                    .await
                    .unwrap();
                assert!(columns.next().await.unwrap().is_none());
            });
        }

        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            let metadata =
                s.rt.block_on(async {
                    let mut rows = s
                        .conn
                        .query(
                            "SELECT request_priority, request_ttl, request_supersedes,
                                    request_dedup_idle
                             FROM messages WHERE id = 2",
                            (),
                        )
                        .await?;
                    let row = rows
                        .next()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("legacy current idle row missing"))?;
                    Ok::<_, anyhow::Error>((
                        row.get::<String>(0)?,
                        row.get::<i64>(1)?,
                        row.get::<i64>(2)?,
                        row.get::<i64>(3)?,
                    ))
                })
                .unwrap();
            assert_eq!(metadata, ("urgent".to_string(), 0, 0, 1));
            assert_eq!(superseded_by_of(&s, "b", 1), Some(2));
        }

        let s = LibsqlStore::open(&cfg).unwrap();
        assert_eq!(
            s.send_configured_idempotent(
                "a",
                "b",
                None,
                "waiting again",
                Some("event:legacy-idle-2"),
                Some("trace:legacy-retry"),
                Some("urgent"),
                None,
                0,
                true,
            )
            .unwrap(),
            (2, false)
        );
        assert_eq!(s.total_messages().unwrap(), 2);
        assert_eq!(superseded_by_of(&s, "b", 1), Some(2));
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configured_reply_ttl_replays_after_parent_removal_libsql() {
        let s = mem();
        let parent = s
            .send("a", "b", Some("root"), "question", None, None)
            .unwrap();
        let (reply, created) = s
            .reply_configured_idempotent(
                "b",
                parent,
                "answer",
                Some("event:configured-reply"),
                Some("urgent"),
                3_600,
                None,
            )
            .unwrap();
        assert!(created);
        let stored = s
            .message_by_idempotency_key("event:configured-reply")
            .unwrap()
            .unwrap();
        assert_eq!(stored.in_reply_to, Some(parent));
        assert_eq!(stored.priority, "urgent");
        assert_eq!(stored.request_priority.as_deref(), Some("urgent"));
        assert_eq!(
            stored.expires_at,
            Some(crate::model::expiry_from_ttl(stored.ts, 3_600))
        );
        assert_eq!(
            s.reply_configured_idempotent(
                "b",
                parent,
                "answer",
                Some("event:configured-reply"),
                Some("urgent"),
                3_600,
                None,
            )
            .unwrap(),
            (reply, false)
        );
        assert!(s
            .reply_configured_idempotent(
                "b",
                parent,
                "answer",
                Some("event:configured-reply"),
                Some("urgent"),
                1_800,
                None,
            )
            .is_err());

        s.rt.block_on(async {
            s.conn
                .execute(
                    "DELETE FROM messages WHERE id = ?1",
                    params(vec![parent.into()]),
                )
                .await
        })
        .unwrap();
        assert_eq!(
            s.reply_configured_idempotent(
                "b",
                parent,
                "answer",
                Some("event:configured-reply"),
                Some("urgent"),
                3_600,
                None,
            )
            .unwrap(),
            (reply, false)
        );
        assert_eq!(s.total_messages().unwrap(), 1);
        assert_eq!(
            s.message_by_idempotency_key("event:configured-reply")
                .unwrap()
                .unwrap()
                .id,
            reply
        );
    }

    #[test]
    fn migration_refuses_to_strip_key_bound_by_v2_signature_libsql() {
        let dir = std::env::temp_dir().join(format!(
            "weave-v2-key-collision-libsql-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("collision.db");
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            s.rt.block_on(async {
                s.conn
                    .execute(
                        "INSERT INTO messages
                            (ts, sender, recipient, body, idempotency_key)
                         VALUES (1, 'a', 'b', 'accepted', 'event:v2-collision')",
                        (),
                    )
                    .await?;
                s.conn
                    .execute(
                        "INSERT INTO outbox
                            (ts, to_peer, to_host, from_peer, body, sig, idempotency_key)
                         VALUES (1, 'b', '', 'a', 'queued', 'v2:bound', 'event:v2-collision')",
                        (),
                    )
                    .await?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap();
        }

        let err = LibsqlStore::open(&cfg).err().expect("migration must fail");
        assert!(err.to_string().contains("signed v2 outbox row"));
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        let preserved = ro
            .rt
            .block_on(async {
                let mut rows = ro
                    .conn
                    .query(
                        "SELECT sig, idempotency_key FROM outbox WHERE body = 'queued'",
                        (),
                    )
                    .await?;
                let row = rows.next().await?.expect("queued row remains");
                Ok::<_, anyhow::Error>((row.get::<String>(0)?, row.get::<String>(1)?))
            })
            .unwrap();
        assert_eq!(
            preserved,
            ("v2:bound".to_string(), "event:v2-collision".to_string())
        );
        drop(ro);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v2_idempotency_migration_handles_null_later_and_safe_earlier_rows_libsql() {
        for (label, rows, must_fail) in [
            (
                "null",
                vec![("v2:null", None), ("", Some("event:other"))],
                true,
            ),
            (
                "later-v2",
                vec![
                    ("legacy", Some("event:dup")),
                    ("v2:later", Some("event:dup")),
                ],
                true,
            ),
            (
                "earlier-v2",
                vec![
                    ("v2:earlier", Some("event:dup")),
                    ("legacy", Some("event:dup")),
                ],
                false,
            ),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "weave-v2-key-{label}-libsql-{}-{}",
                std::process::id(),
                now()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("migration.db");
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            {
                let store = LibsqlStore::open(&cfg).unwrap();
                store
                    .rt
                    .block_on(async {
                        store
                            .conn
                            .execute("DROP INDEX idx_outbox_idempotency_key", ())
                            .await?;
                        for (index, (signature, key)) in rows.iter().enumerate() {
                            store
                                .conn
                                .execute(
                                    "INSERT INTO outbox
                                        (ts, to_peer, to_host, from_peer, body, sig, idempotency_key)
                                     VALUES (?1, 'b', '', 'a', ?2, ?3, ?4)",
                                    params(vec![
                                        (index as i64 + 1).into(),
                                        format!("row-{index}").into(),
                                        (*signature).into(),
                                        (*key).into(),
                                    ]),
                                )
                                .await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    })
                    .unwrap();
            }

            let reopened = LibsqlStore::open(&cfg);
            if must_fail {
                let error = reopened.err().expect("signed v2 key loss must fail");
                assert!(error.to_string().contains("signed v2 outbox row"));
            } else {
                let store = reopened.expect("earlier signed row retains its key");
                let keys = store
                    .rt
                    .block_on(async {
                        let mut rows = store
                            .conn
                            .query("SELECT idempotency_key FROM outbox ORDER BY id", ())
                            .await?;
                        let mut keys = Vec::new();
                        while let Some(row) = rows.next().await? {
                            keys.push(row.get::<Option<String>>(0)?);
                        }
                        Ok::<_, anyhow::Error>(keys)
                    })
                    .unwrap();
                assert_eq!(keys, vec![Some("event:dup".to_string()), None]);
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn keyed_mutations_replay_exactly_once_across_reopen_libsql() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("weave-keyed-libsql-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };

        let s = LibsqlStore::open(&cfg).unwrap();
        let (sent, created) = s
            .send_idempotent(
                "a",
                "b",
                None,
                "hello",
                Some("event:send"),
                Some("trace:first-send"),
            )
            .unwrap();
        assert!(created);
        assert_eq!(
            s.send_idempotent(
                "a",
                "b",
                None,
                "hello",
                Some("event:send"),
                Some("trace:retry-send"),
            )
            .unwrap(),
            (sent, false)
        );
        assert_eq!(
            s.message_by_idempotency_key("event:send")
                .unwrap()
                .unwrap()
                .trace_id
                .as_deref(),
            Some("trace:first-send")
        );
        assert!(s
            .send_idempotent("a", "b", None, "different", Some("event:send"), None)
            .is_err());

        let (cid, question_id, created) = s
            .ask_idempotent(
                "a",
                "b",
                None,
                "question",
                AskKind::FreeText,
                None,
                None,
                Some("event:ask"),
            )
            .unwrap();
        assert!(created);
        assert!(s
            .send_idempotent("a", "b", None, "question", Some("event:ask"), None,)
            .is_err());
        drop(s);

        let s = LibsqlStore::open(&cfg).unwrap();
        assert_eq!(
            s.ask_idempotent(
                "a",
                "b",
                None,
                "question",
                AskKind::FreeText,
                None,
                None,
                Some("event:ask"),
            )
            .unwrap(),
            (cid.clone(), question_id, false)
        );
        let (answer_id, created) = s
            .answer_idempotent("b", &cid, "answer", Some("event:answer"))
            .unwrap();
        assert!(created);
        assert_eq!(
            s.answer_idempotent("b", &cid, "answer", Some("event:answer"))
                .unwrap(),
            (answer_id, false)
        );
        assert!(s
            .reply_idempotent("b", question_id, "answer", Some("event:answer"))
            .is_err());
        assert!(s
            .answer_idempotent("b", &cid, "second", Some("event:answer-2"))
            .is_err());

        let root = s.send("a", "b", None, "root", None, None).unwrap();
        let (reply_id, created) = s
            .reply_idempotent("b", root, "reply", Some("event:reply"))
            .unwrap();
        assert!(created);
        assert_eq!(
            s.reply_idempotent("b", root, "reply", Some("event:reply"))
                .unwrap(),
            (reply_id, false)
        );
        assert!(s
            .reply_idempotent("b", root, "reply", Some("event:ask"))
            .is_err());
        let (intent_id, created) = s
            .enqueue_intent_idempotent(
                "b",
                "host-b",
                "a",
                None,
                "intent",
                "",
                Some("event:intent"),
                Some("trace:first-intent"),
                None,
                0,
            )
            .unwrap();
        assert!(created);
        assert_eq!(
            s.enqueue_intent_idempotent(
                "b",
                "host-b",
                "a",
                None,
                "intent",
                "",
                Some("event:intent"),
                Some("trace:retry-intent"),
                None,
                0,
            )
            .unwrap(),
            (intent_id, false)
        );
        assert!(s
            .enqueue_intent_idempotent(
                "b",
                "host-b",
                "a",
                None,
                "different",
                "",
                Some("event:intent"),
                None,
                None,
                0,
            )
            .is_err());
        assert_eq!(s.outbox_all(10).unwrap().len(), 1);
        assert_eq!(
            s.outbox_all(10).unwrap()[0].trace_id.as_deref(),
            Some("trace:first-intent")
        );
        assert_eq!(s.list_asks("a", AskRole::Any, 10).unwrap().len(), 1);
        assert_eq!(s.all_messages(20).unwrap().len(), 5);
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// WL-084 (dual-backend parity): mirror of the sqlite backend's
    /// `client_session_roundtrips_and_preserves_on_empty` — the launcher-session
    /// key roundtrips, '' preserves, a non-empty key overwrites, and
    /// `get_peer_by_client_session` never matches the empty key.
    #[test]
    fn client_session_roundtrips_and_preserves_on_empty_libsql() {
        let s = mem();
        s.register_peer_full(
            "a",
            "tmux",
            "%1",
            "",
            Some("/x"),
            Some(11),
            "h",
            "",
            "",
            "",
            "default",
            None,
            "sid-A",
        )
        .unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().client_session, "sid-A");
        assert_eq!(
            s.get_peer_by_client_session("sid-A").unwrap().unwrap().name,
            "a"
        );
        let cert = s.get_birth_cert("a").unwrap();
        s.register_peer_full(
            "a",
            "tmux",
            "%2",
            "",
            Some("/x"),
            Some(11),
            "h",
            "",
            "",
            "",
            "default",
            cert.as_deref(),
            "",
        )
        .unwrap();
        assert_eq!(
            s.get_peer("a").unwrap().unwrap().client_session,
            "sid-A",
            "'' must mean preserve, not wipe"
        );
        s.register_peer_full(
            "a",
            "tmux",
            "%3",
            "",
            Some("/x"),
            Some(12),
            "h",
            "",
            "",
            "",
            "default",
            cert.as_deref(),
            "sid-B",
        )
        .unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().client_session, "sid-B");
        assert!(s.get_peer_by_client_session("sid-A").unwrap().is_none());
        s.register_peer("legacy", "tmux", "%4", "", None).unwrap();
        assert!(s.get_peer_by_client_session("").unwrap().is_none());

        let oversized = "s".repeat(MAX_IDENT + 1);
        assert!(s
            .register_peer_full(
                "rejected",
                "tmux",
                "%5",
                "",
                None,
                Some(13),
                "h",
                "",
                "",
                "",
                "default",
                None,
                &oversized,
            )
            .is_err());
        assert!(s.get_peer("rejected").unwrap().is_none());
        assert!(s.get_peer_by_client_session("sid\nraw").is_err());
        assert!(s.get_peer_by_client_session(" sid-B").is_err());
        assert!(s.get_peer_by_client_session("sid-B ").is_err());
    }

    /// WL-047 (dual-backend parity): byte-identical mirror of the sqlite backend's
    /// `register_peer_full_binds_supplied_cert_else_mints` — the libsql new-peer
    /// INSERT must honor a SUPPLIED birth cert (persist verbatim) and mint when None.
    /// Any divergence from the sqlite branch is the dual-backend bug class WL-047 guards.
    #[test]
    fn register_peer_full_binds_supplied_cert_else_mints() {
        let s = mem();
        // Supplied cert path: a freshly minted valid cert must persist EXACTLY.
        let cert = mint_birth_cert().unwrap();
        let returned = s
            .register_peer_full(
                "spawned",
                "tmux",
                "%9",
                "",
                Some("/w"),
                None,
                "h",
                "",
                "",
                "",
                "default",
                Some(&cert),
                "",
            )
            .unwrap();
        assert_eq!(returned, cert, "register returns the supplied cert");
        assert_eq!(
            s.get_birth_cert("spawned").unwrap().unwrap(),
            cert,
            "the supplied cert is persisted verbatim on the new-peer INSERT"
        );
        // None path (backward-compat): a fresh peer mints its own distinct cert.
        let minted = s
            .register_peer_full(
                "auto", "tmux", "%1", "", None, None, "h", "", "", "", "default", None, "",
            )
            .unwrap();
        assert!(check_birth_cert(&minted).is_ok(), "minted cert is valid");
        assert_ne!(minted, cert, "the None path mints a fresh, distinct cert");
        assert_eq!(s.get_birth_cert("auto").unwrap().unwrap(), minted);
    }

    #[test]
    fn ask_open_answer_ack_roundtrip() {
        let s = mem();
        let (cid, qid) = s
            .ask(
                "a",
                "b",
                Some("help"),
                "what time?",
                AskKind::FreeText,
                None,
                None,
            )
            .unwrap();
        assert!(crate::model::ask_id_valid(&cid));
        let (b_in, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(b_in.iter().any(|m| m.id == qid && m.sender == "a"));
        assert_eq!(s.get_ask(&cid).unwrap().unwrap().state, AskState::Open);

        let aid = s.answer("b", &cid, "3pm").unwrap();
        let (a_in, _) = s.inbox("a", true, false, 50).unwrap();
        let ans = a_in.iter().find(|m| m.id == aid).expect("a got answer");
        assert_eq!(ans.sender, "b");
        assert_eq!(ans.recipient, "a");
        assert_eq!(ans.in_reply_to, Some(qid));
        let ask = s.get_ask(&cid).unwrap().unwrap();
        assert_eq!(ask.state, AskState::Answered);
        assert_eq!(ask.answer_msg_id, Some(aid));

        s.ack("b", &cid, Some("done")).unwrap();
        let ask = s.get_ask(&cid).unwrap().unwrap();
        assert_eq!(ask.state, AskState::Acked);
        assert!(ask.closed_ts.is_some());
        assert_eq!(ask.close_note.as_deref(), Some("done"));
    }

    #[test]
    fn ask_lifecycle_is_monotonic() {
        let s = mem();
        let (cid, _) = s
            .ask("a", "b", None, "q", AskKind::FreeText, None, None)
            .unwrap();
        s.ack("b", &cid, None).unwrap();
        assert!(s.ack("b", &cid, None).is_err());
        assert!(s.answer("b", &cid, "late").is_err());
        assert!(s.ack("b", "ask_999_1", None).is_err());
        assert!(s.get_ask("ask_999_1").unwrap().is_none());
    }

    #[test]
    fn ask_owner_checks_and_caps() {
        let s = mem();
        let (cid, _) = s
            .ask("a", "b", None, "q", AskKind::FreeText, None, None)
            .unwrap();
        assert!(s.answer("a", &cid, "self").is_err());
        assert!(s.ack("a", &cid, None).is_err());
        assert!(s
            .ask("a", "all", None, "q", AskKind::FreeText, None, None)
            .is_err());
        let big = "x".repeat(crate::store::MAX_BODY + 1);
        assert!(s
            .ask("a", "b", None, &big, AskKind::FreeText, None, None)
            .is_err());
        assert!(s.answer("b", "ask;rm -rf", "x").is_err());
        assert!(s.get_ask("bad id").is_err());
    }

    #[test]
    fn ask_reply_to_chains_and_closes_prior() {
        let s = mem();
        let (c1, q1) = s
            .ask(
                "a",
                "b",
                Some("topic"),
                "first?",
                AskKind::FreeText,
                None,
                None,
            )
            .unwrap();
        s.answer("b", &c1, "first-ans").unwrap();
        let (c2, q2) = s
            .ask(
                "a",
                "b",
                None,
                "second?",
                AskKind::FreeText,
                None,
                Some(&c1),
            )
            .unwrap();
        assert_eq!(s.get_ask(&c1).unwrap().unwrap().state, AskState::Acked);
        assert_eq!(
            s.get_ask(&c2).unwrap().unwrap().reply_to.as_deref(),
            Some(c1.as_str())
        );
        let thread = s.thread(q1, 50).unwrap();
        assert!(thread.iter().any(|m| m.id == q2));
        assert!(s
            .ask(
                "a",
                "b",
                None,
                "x",
                AskKind::FreeText,
                None,
                Some("ask_404_1")
            )
            .is_err());
    }

    #[test]
    fn expiry_prunes_ask_and_clears_surviving_reply_edges_libsql() {
        let s = mem();
        let (parent, parent_question) = s
            .ask("a", "b", None, "first?", AskKind::FreeText, None, None)
            .unwrap();
        let (child, child_question) = s
            .ask(
                "a",
                "b",
                None,
                "second?",
                AskKind::FreeText,
                None,
                Some(&parent),
            )
            .unwrap();
        s.set_message_expiry(parent_question, now().saturating_sub(1))
            .unwrap();
        assert_eq!(s.sweep_expired_messages().unwrap(), 1);
        assert!(s.get_ask(&parent).unwrap().is_none());
        assert_eq!(s.get_ask(&child).unwrap().unwrap().reply_to, None);
        let child_message = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == child_question)
            .unwrap();
        assert_eq!(child_message.in_reply_to, None);
    }

    #[test]
    fn list_asks_role_filtering() {
        let s = mem();
        let (c1, _) = s
            .ask("a", "b", None, "q1", AskKind::FreeText, None, None)
            .unwrap();
        let (c2, _) = s
            .ask("b", "a", None, "q2", AskKind::FreeText, None, None)
            .unwrap();
        assert_eq!(s.list_asks("a", AskRole::Asker, 50).unwrap()[0].id, c1);
        assert_eq!(s.list_asks("a", AskRole::Askee, 50).unwrap()[0].id, c2);
        assert_eq!(s.list_asks("a", AskRole::Any, 50).unwrap().len(), 2);
    }

    /// WL-040b libsql parity: `import_ask` materializes an ANSWERED ask out-of-order
    /// with the positional 15-column INSERT mapping cleanly back through `row_to_ask`,
    /// and dedups on (asker, askee, question) so re-import is a no-op.
    #[test]
    fn import_ask_materializes_answered_and_is_idempotent_libsql() {
        let s = mem();
        let q = s
            .send("a", "b", Some("subj"), "question?", None, None)
            .unwrap();
        let q_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == q)
            .unwrap()
            .ts;
        let (ans, _) = s
            .reply_configured_idempotent("b", q, "answer!", None, None, 0, None)
            .unwrap();
        let answer_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == ans)
            .unwrap()
            .ts;
        let id = crate::model::new_ask_id(q);
        assert!(s
            .import_ask(
                &id,
                q,
                Some(ans),
                "a",
                "b",
                Some("subj"),
                AskState::Answered,
                AskKind::Choice,
                Some("yes\nno"),
                None,
                None,
                q_ts,
                answer_ts,
                None,
                None,
            )
            .unwrap());
        let got = s.get_ask(&id).unwrap().unwrap();
        assert_eq!(got.state, AskState::Answered);
        assert_eq!(got.kind, AskKind::Choice);
        assert_eq!(got.options.as_deref(), Some("yes\nno"));
        assert_eq!(got.answer_msg_id, Some(ans));
        assert_eq!(got.closed_ts, None);
        // Idempotent skip.
        assert!(!s
            .import_ask(
                &crate::model::new_ask_id(q),
                q,
                Some(ans),
                "a",
                "b",
                Some("subj"),
                AskState::Answered,
                AskKind::Choice,
                Some("yes\nno"),
                None,
                None,
                q_ts,
                answer_ts,
                None,
                None,
            )
            .unwrap());
        let mismatch = s
            .import_ask(
                &crate::model::new_ask_id(q),
                q,
                Some(ans),
                "a",
                "b",
                Some("subj"),
                AskState::Acked,
                AskKind::Choice,
                Some("yes\nno"),
                None,
                Some("different terminal state"),
                q_ts,
                answer_ts + 1,
                Some(answer_ts + 1),
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(
            mismatch.contains("different content or lifecycle state"),
            "a matching triple must not mask lifecycle drift: {mismatch}"
        );
        assert_eq!(s.list_asks("a", AskRole::Any, 50).unwrap().len(), 1);
    }

    /// WL-040b libsql parity: `import_ask` materializes an ACKED ask + `import_ask_group`
    /// replays a parent anchor with the child's `parent_id` linked; `ask_many_result`
    /// reads the group back; `list_ask_groups` returns it by id.
    #[test]
    fn import_ask_acked_and_group_libsql() {
        let s = mem();
        let q = s
            .send("a", "b", Some("poll"), "yes or no?", None, None)
            .unwrap();
        let opened = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == q)
            .unwrap()
            .ts;
        let pid = crate::model::new_ask_many_id(500);
        assert!(s
            .import_ask_group(&pid, "a", Some("poll"), "yes or no?", opened, 2)
            .unwrap());
        assert!(!s
            .import_ask_group(&pid, "a", Some("poll"), "yes or no?", opened, 2)
            .unwrap());
        let id = crate::model::new_ask_id(q);
        assert!(s
            .import_ask(
                &id,
                q,
                None,
                "a",
                "b",
                Some("poll"),
                AskState::Acked,
                AskKind::FreeText,
                None,
                None,
                Some("closing note"),
                opened,
                opened,
                Some(opened),
                Some(&pid),
            )
            .unwrap());
        let got = s.get_ask(&id).unwrap().unwrap();
        assert_eq!(got.state, AskState::Acked);
        assert_eq!(got.closed_ts, Some(opened));
        assert_eq!(got.close_note.as_deref(), Some("closing note"));
        assert_eq!(got.parent_id.as_deref(), Some(pid.as_str()));
        let res = s.ask_many_result(&pid, None).unwrap().unwrap();
        assert_eq!(res.target_count, 2);
        assert_eq!(res.acked, 1);
        let groups = s
            .list_ask_groups(&[pid.clone(), "askm_999_1".to_string()])
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].parent_id, pid);
    }

    /// WL-040b libsql parity: store-seam re-validation rejects a hostile askee, a
    /// malformed ask id, and an oversized options payload before the INSERT.
    #[test]
    fn import_ask_rejects_malformed_inputs_libsql() {
        let s = mem();
        let q = s.send("a", "b", None, "q", None, None).unwrap();
        assert!(s
            .import_ask(
                "ask_1_1",
                q,
                None,
                "a",
                "b\u{7}c",
                None,
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
        assert!(s
            .import_ask(
                "bad id",
                q,
                None,
                "a",
                "b",
                None,
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
        let big = "x".repeat(crate::store::MAX_BODY + 1);
        assert!(s
            .import_ask(
                "ask_1_1",
                q,
                None,
                "a",
                "b",
                None,
                AskState::Open,
                AskKind::FreeText,
                Some(&big),
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
    }

    #[test]
    fn import_ask_rejects_incoherent_lifecycle_links_aliases_and_groups_libsql() {
        let s = mem();
        let q = s
            .send("a", "b", Some("topic"), "question", None, None)
            .unwrap();
        let q_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == q)
            .unwrap()
            .ts;
        let detached = s
            .send("b", "a", Some("Re: topic"), "detached", None, None)
            .unwrap();
        let detached_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == detached)
            .unwrap()
            .ts;
        assert!(s
            .import_ask(
                "ask_1_90",
                q,
                None,
                "a",
                "b",
                Some("topic"),
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts + 1,
                q_ts + 1,
                None,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("question timestamp"));
        let (linked, _) = s
            .reply_configured_idempotent("b", q, "linked", None, None, 0, None)
            .unwrap();
        let linked_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == linked)
            .unwrap()
            .ts;
        assert!(s
            .import_ask(
                "ask_1_91",
                q,
                Some(linked),
                "a",
                "b",
                Some("topic"),
                AskState::Answered,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts,
                linked_ts + 1,
                None,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("answer timestamp"));
        assert!(s
            .import_ask(
                "ask_1_101",
                q,
                Some(detached),
                "a",
                "b",
                Some("topic"),
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts,
                q_ts,
                None,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("open ask has incoherent lifecycle"));
        assert!(s
            .import_ask(
                "ask_1_102",
                q,
                Some(detached),
                "a",
                "b",
                Some("topic"),
                AskState::Answered,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts,
                detached_ts,
                None,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("answer is incoherent"));
        assert!(s
            .import_ask(
                "ask_1_103",
                q,
                None,
                "a",
                "b",
                Some("topic"),
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts,
                q_ts,
                None,
                None,
            )
            .unwrap());
        assert!(s
            .import_ask(
                "ask_1_104",
                q,
                None,
                "a",
                "c",
                Some("topic"),
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                q_ts,
                q_ts,
                None,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("already claimed"));
        let orphan_q = s.send("a", "c", None, "orphan", None, None).unwrap();
        let orphan_ts = s
            .all_messages(10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == orphan_q)
            .unwrap()
            .ts;
        assert!(s
            .import_ask(
                "ask_2_105",
                orphan_q,
                None,
                "a",
                "c",
                None,
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                orphan_ts,
                orphan_ts,
                None,
                Some("askm_404_1"),
            )
            .unwrap_err()
            .to_string()
            .contains("group 'askm_404_1' is missing"));
    }

    /// libsql parity: `has_open_asks` matches the sqlite semantics.
    #[test]
    fn has_open_asks_libsql() {
        let s = mem();
        let (c1, _) = s
            .ask("a", "b", None, "q1", AskKind::FreeText, None, None)
            .unwrap();
        assert!(s.has_open_asks("b").unwrap(), "b is askee of an open ask");
        assert!(!s.has_open_asks("a").unwrap(), "a is asker");
        s.answer("b", &c1, "ans").unwrap();
        assert!(!s.has_open_asks("b").unwrap(), "answered, no longer open");
    }

    /// libsql parity: `list_asks` is bounded (clamped to MAX_LIMIT), and
    /// `ask_for_message` resolves both the question and answer ends.
    #[test]
    fn list_asks_bounded_and_ask_for_message_libsql() {
        let s = mem();
        let (cid, qid) = s
            .ask("a", "b", None, "q", AskKind::FreeText, None, None)
            .unwrap();
        let aid = s.answer("b", &cid, "a").unwrap();
        assert_eq!(
            s.ask_for_message(qid).unwrap().as_deref(),
            Some(cid.as_str())
        );
        assert_eq!(
            s.ask_for_message(aid).unwrap().as_deref(),
            Some(cid.as_str())
        );
        let mid = s.send("a", "b", None, "plain", None, None).unwrap();
        assert_eq!(s.ask_for_message(mid).unwrap(), None);
        let huge = s.list_asks("a", AskRole::Any, i64::MAX).unwrap();
        assert!(
            huge.len() <= MAX_LIMIT as usize,
            "list_asks must clamp to MAX_LIMIT"
        );
    }

    /// libsql write-trap parity: a `read_only` handle TRAPS every mutating ask op
    /// (`ask`/`answer`/`ack`) at the `guard_writable` boundary (owner-only-writes),
    /// while the read paths (`get_ask`/`list_asks`/`ask_for_message`) still work.
    /// Mirrors `open_readonly_reads_but_cannot_write_libsql` for the ask surface.
    #[test]
    fn ask_writes_trap_on_readonly_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-ask-ro-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.db");

        // Seed an ask via a normal RW open, then drop the handle.
        let cid = {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            let (cid, _) = rw
                .ask("a", "b", None, "q", AskKind::FreeText, None, None)
                .unwrap();
            cid
        };

        let ro = LibsqlStore::open_readonly(&path).unwrap();
        // Reads work through the read-only handle.
        assert_eq!(ro.get_ask(&cid).unwrap().unwrap().state, AskState::Open);
        assert_eq!(ro.list_asks("a", AskRole::Any, 50).unwrap().len(), 1);
        // All three mutating ops trap (never a silent foreign write, never a panic).
        assert!(
            ro.ask("a", "b", None, "intruder", AskKind::FreeText, None, None)
                .is_err(),
            "ask through a read-only handle must trap"
        );
        assert!(
            ro.answer("b", &cid, "intruder").is_err(),
            "answer through a read-only handle must trap"
        );
        assert!(
            ro.ack("b", &cid, None).is_err(),
            "ack through a read-only handle must trap"
        );
        // The failed writes were no-ops: the ask is still open.
        assert_eq!(ro.get_ask(&cid).unwrap().unwrap().state, AskState::Open);
    }

    /// libsql parity: a legacy DB predating the `asks` table gains it idempotently
    /// on open (mirror of the sqlite `legacy_db_gains_asks_table`), a full lifecycle
    /// then works, and re-opening is a no-op that retains the acked ask.
    #[test]
    fn legacy_db_gains_asks_table_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-asks-legacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // A pre-P1 store: a messages table + a row, but NO asks table.
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let s = LibsqlStore::open(&cfg).unwrap();
            s.send("a", "b", None, "pre-existing", None, None).unwrap();
            // Drop the asks table to simulate a DB that predates the migration.
            s.rt.block_on(async { s.conn.execute("DROP TABLE asks", ()).await })
                .unwrap();
            let exists =
                s.rt.block_on(async {
                    let mut rows = s
                        .conn
                        .query(
                            "SELECT name FROM sqlite_master WHERE type='table' AND name='asks'",
                            (),
                        )
                        .await?;
                    rows.next().await
                })
                .unwrap()
                .is_some();
            assert!(!exists, "fixture must predate the asks table");
        }
        // Re-open runs the idempotent migration: the table is recreated, a full
        // lifecycle works, and the pre-existing message survived.
        let cid = {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let s = LibsqlStore::open(&cfg).unwrap();
            let (cid, _) = s
                .ask("a", "b", Some("subj"), "q?", AskKind::FreeText, None, None)
                .unwrap();
            s.answer("b", &cid, "ans").unwrap();
            s.ack("b", &cid, Some("closed")).unwrap();
            assert_eq!(s.get_ask(&cid).unwrap().unwrap().state, AskState::Acked);
            let (rows, _) = s.inbox("b", true, false, 50).unwrap();
            assert!(
                rows.iter().any(|m| m.body == "pre-existing"),
                "pre-existing message survived migration"
            );
            cid
        };
        // Re-open once more: idempotent, prior acked ask retained.
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(s.get_ask(&cid).unwrap().unwrap().state, AskState::Acked);
        }
    }

    /// libsql parity (P2): `create_ask_many` opens parent + N children; aggregate
    /// tracks the UNCHANGED P1 lifecycle; best-effort per child + caps; totality holds.
    #[test]
    fn ask_many_create_aggregate_and_caps_libsql() {
        let s = mem();
        let out = s
            .create_ask_many("a", &["b".into(), "c".into(), "d".into()], Some("t"), "q?")
            .unwrap();
        assert!(crate::model::ask_many_id_valid(&out.parent_id));
        assert_eq!(out.children.len(), 3);
        let cids: Vec<String> = out
            .children
            .iter()
            .map(|(_, r)| r.as_ref().unwrap().clone())
            .collect();
        for (peer, res) in &out.children {
            let ask = s.get_ask(res.as_ref().unwrap()).unwrap().unwrap();
            assert_eq!(ask.parent_id.as_deref(), Some(out.parent_id.as_str()));
            assert_eq!(ask.askee, *peer);
        }
        s.answer("b", &cids[0], "yes").unwrap();
        s.ack("c", &cids[1], None).unwrap();
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!((r.answered, r.acked, r.pending, r.failed), (1, 1, 1, 0));
        assert_eq!(r.answered + r.acked + r.pending + r.failed, r.target_count);
        assert_eq!(r.state, crate::model::AskManyState::Pending);
        s.ack("d", &cids[2], None).unwrap();
        assert_eq!(
            s.ask_many_result(&out.parent_id, None)
                .unwrap()
                .unwrap()
                .state,
            crate::model::AskManyState::Complete
        );

        // Best-effort per child + de-dup + caps.
        let out = s
            .create_ask_many("a", &["b".into(), "all".into(), "b".into()], None, "q")
            .unwrap();
        // de-dup collapses the repeated "b"; "all" is a per-child reject.
        assert_eq!(out.children.len(), 2);
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!(r.failed, 1);
        assert_eq!(r.pending, 1);
        assert!(s.create_ask_many("a", &[], None, "q").is_err());
        let many: Vec<String> = (0..MAX_ASK_MANY_TARGETS + 1)
            .map(|i| format!("p{i}"))
            .collect();
        assert!(s.create_ask_many("a", &many, None, "q").is_err());
        assert!(s.ask_many_result("askm;rm", None).is_err());
        assert!(s.ask_many_result("askm_1_2", None).unwrap().is_none());
    }

    /// libsql write-trap parity (P2): `create_ask_many` traps on a read-only handle
    /// (owner-only-writes) while `ask_many_result` reads work.
    #[test]
    fn ask_many_write_traps_on_readonly_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-askmany-ro-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.db");
        let parent = {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            rw.create_ask_many("a", &["b".into()], None, "q")
                .unwrap()
                .parent_id
        };
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(ro.ask_many_result(&parent, None).unwrap().is_some());
        assert!(
            ro.create_ask_many("a", &["c".into()], None, "intruder")
                .is_err(),
            "create_ask_many through a read-only handle must trap"
        );
    }

    /// libsql parity (P2): a legacy P1-era DB whose `asks` lacks `parent_id` and
    /// whose `messages` table predates `idempotency_key` upgrades in place. The
    /// message column is installed before the keyed-ask backfill, parent_id is added
    /// NULL on the old row, ask_groups is created, and a fresh fanout then works.
    #[test]
    fn legacy_asks_gains_parent_id_and_ask_groups_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-askmany-legacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // Build a genuine P1-era, pre-WL-026 store: `messages` has no
        // idempotency_key, `asks` has no parent_id/request-shape columns, and there
        // is no ask_groups table. Opening it must not reference the missing message
        // column before the additive WL-026 migration runs.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Builder::new_local(&path).build().await.unwrap();
                let conn = db.connect().unwrap();
                conn.execute(
                    "CREATE TABLE messages (
                        id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                        sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT,
                        body TEXT NOT NULL, in_reply_to INTEGER
                     )",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "CREATE TABLE asks (
                        id TEXT PRIMARY KEY, question_msg_id INTEGER NOT NULL,
                        answer_msg_id INTEGER, asker TEXT NOT NULL, askee TEXT NOT NULL,
                        subject TEXT, state TEXT NOT NULL, reply_to TEXT, close_note TEXT,
                        opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL,
                        closed_ts INTEGER
                     )",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO messages (ts, sender, recipient, subject, body)
                     VALUES (1, 'a', 'b', NULL, 'q')",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO asks
                         (id, question_msg_id, asker, askee, state, opened_ts, updated_ts)
                     VALUES ('ask_1_legacy', 1, 'a', 'b', 'open', 1, 1)",
                    (),
                )
                .await
                .unwrap();
            });
        }
        // Re-open runs the migration: parent_id (NULL) added, ask_groups recreated.
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let s = LibsqlStore::open(&cfg).unwrap();
            let old = s.get_ask("ask_1_legacy").unwrap().unwrap();
            assert_eq!(old.parent_id, None);
            let (has_idempotency_key, request_shape) =
                s.rt.block_on(async {
                    let mut columns = s
                        .conn
                        .query(
                            "SELECT 1 FROM pragma_table_info('messages')
                              WHERE name='idempotency_key'",
                            (),
                        )
                        .await?;
                    let has_idempotency_key = columns.next().await?.is_some();
                    let mut rows = s
                        .conn
                        .query(
                            "SELECT request_subject, request_subject_provided
                               FROM asks WHERE id = 'ask_1_legacy'",
                            (),
                        )
                        .await?;
                    let row = rows
                        .next()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("migrated legacy ask missing"))?;
                    let shape = (row.get::<Option<String>>(0)?, row.get::<Option<i64>>(1)?);
                    Ok::<_, anyhow::Error>((has_idempotency_key, shape))
                })
                .unwrap();
            assert!(
                has_idempotency_key,
                "idempotency schema must exist before the ask replay backfill"
            );
            assert_eq!(
                request_shape,
                (None, None),
                "an unkeyed legacy ask must not be misclassified as keyed"
            );
            let out = s.create_ask_many("a", &["c".into()], None, "fan?").unwrap();
            assert_eq!(
                s.ask_many_result(&out.parent_id, None)
                    .unwrap()
                    .unwrap()
                    .target_count,
                1
            );
        }
    }

    #[test]
    fn send_and_read_tracking() {
        let s = mem();
        s.send("desktop", "envctl", Some("hi"), "body1", None, None)
            .unwrap();
        s.send("desktop", "all", None, "bcast", None, None).unwrap();

        let (rows, remaining) = s.inbox("envctl", false, true, 50).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(remaining, 0);

        let (rows2, _) = s.inbox("envctl", false, true, 50).unwrap();
        assert_eq!(rows2.len(), 0);

        let (mine, _) = s.inbox("desktop", false, true, 50).unwrap();
        assert_eq!(mine.len(), 0);
    }

    #[test]
    fn peer_upsert_and_presence() {
        let s = mem();
        s.register_peer("envctl", "zellij", "envctl", "", Some("/home/x/envctl"))
            .unwrap();
        s.register_peer(
            "envctl",
            "tmux",
            "%4",
            "/run/kitty.sock",
            Some("/home/x/envctl"),
        )
        .unwrap();
        let p = s.get_peer("envctl").unwrap().unwrap();
        assert_eq!(p.mux, "tmux");
        assert_eq!(p.target, "%4");
        assert_eq!(p.socket, "/run/kitty.sock");
        assert!(is_online(p.last_seen));
        assert!(!is_online(p.last_seen - ONLINE_TTL_SECS - 1));
        assert_eq!(s.list_peers().unwrap().len(), 1);
    }

    #[test]
    fn socket_persists_through_upsert() {
        let s = mem();
        s.register_peer("k", "kitty", "1", "/run/a.sock", Some("/w"))
            .unwrap();
        assert_eq!(s.get_peer("k").unwrap().unwrap().socket, "/run/a.sock");
        // Upsert with a new socket overwrites it.
        s.register_peer("k", "kitty", "1", "/run/b.sock", Some("/w"))
            .unwrap();
        assert_eq!(s.get_peer("k").unwrap().unwrap().socket, "/run/b.sock");
        // list_peers also carries the socket.
        let peers = s.list_peers().unwrap();
        assert_eq!(peers[0].socket, "/run/b.sock");
    }

    #[test]
    fn register_peer_rejects_invalid_name() {
        let s = mem();
        assert!(s.register_peer("", "tmux", "%1", "", None).is_err());
        assert!(s
            .register_peer(&"n".repeat(MAX_IDENT + 1), "tmux", "%1", "", None)
            .is_err());
        assert!(s.register_peer("ok", "tmux", "%1", "", None).is_ok());
    }

    #[test]
    fn touch_peer_refreshes_without_clobbering() {
        let s = mem();
        s.register_peer("envctl", "tmux", "%7", "/run/k.sock", Some("/w"))
            .unwrap();
        // Backdate last_seen, then touch and confirm only last_seen advanced.
        s.backdate_peer("envctl", 100_000).unwrap();
        let before = s.get_peer("envctl").unwrap().unwrap();
        s.touch_peer("envctl").unwrap();
        let after = s.get_peer("envctl").unwrap().unwrap();
        assert!(after.last_seen > before.last_seen);
        assert_eq!(after.mux, "tmux");
        assert_eq!(after.target, "%7");
        assert_eq!(after.socket, "/run/k.sock");
        assert_eq!(after.cwd.as_deref(), Some("/w"));

        // Touching an unknown peer is a silent no-op (no row created).
        s.touch_peer("ghost").unwrap();
        assert!(s.get_peer("ghost").unwrap().is_none());
    }

    #[test]
    fn history_scoped() {
        let s = mem();
        s.send("a", "b", None, "1", None, None).unwrap();
        s.send("b", "a", None, "2", None, None).unwrap();
        s.send("c", "d", None, "x", None, None).unwrap();
        let h = s.history("a", Some("b"), 50).unwrap();
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn all_messages_crosses_identity_scope_explicitly() {
        let s = mem();
        s.send("a", "b", None, "1", None, None).unwrap();
        s.send("c", "d", None, "2", None, None).unwrap();
        assert_eq!(s.history("a", None, 50).unwrap().len(), 1);
        let all = s.all_messages(50).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].body, "1");
        assert_eq!(all[1].body, "2");
    }

    #[test]
    fn reply_addresses_back_and_links() {
        let s = mem();
        let root = s
            .send("a", "b", Some("hi"), "question?", None, None)
            .unwrap();
        let r1 = s.reply("b", root, "answer.").unwrap();

        let (a_inbox, _) = s.inbox("a", true, false, 50).unwrap();
        let reply = a_inbox.iter().find(|m| m.id == r1).expect("a got reply");
        assert_eq!(reply.sender, "b");
        assert_eq!(reply.recipient, "a");
        assert_eq!(reply.subject.as_deref(), Some("Re: hi"));
        assert_eq!(reply.in_reply_to, Some(root));

        // A reply authored by the original sender goes to the other party too,
        // and "Re:" is not stacked.
        let r2 = s.reply("a", r1, "thanks!").unwrap();
        let reply2 = s
            .thread(root, 50)
            .unwrap()
            .into_iter()
            .find(|m| m.id == r2)
            .unwrap();
        assert_eq!(reply2.recipient, "b");
        assert_eq!(reply2.subject.as_deref(), Some("Re: hi"));
    }

    #[test]
    fn thread_collects_transitive_replies_in_order() {
        let s = mem();
        let root = s.send("a", "b", Some("topic"), "m0", None, None).unwrap();
        let c1 = s.reply("b", root, "m1").unwrap();
        let c2 = s.reply("a", c1, "m2").unwrap(); // nested reply-to-a-reply
        let _other = s.send("a", "b", None, "unrelated", None, None).unwrap();

        let thread = s.thread(root, 50).unwrap();
        let ids: Vec<i64> = thread.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![root, c1, c2],
            "root + transitive replies, oldest-first"
        );
        assert!(thread.iter().all(|m| m.body != "unrelated"));
    }

    #[test]
    fn receipts_reports_readers() {
        let s = mem();
        let id = s.send("a", "all", None, "ping", None, None).unwrap();
        assert!(s.receipts(id).unwrap().is_empty(), "nobody has read yet");

        s.inbox("b", false, true, 50).unwrap();
        s.inbox("c", false, true, 50).unwrap();
        let r = s.receipts(id).unwrap();
        let readers: Vec<&str> = r.iter().map(|(name, _)| name.as_str()).collect();
        assert!(readers.contains(&"b"));
        assert!(readers.contains(&"c"));
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|(_, ts)| *ts > 0));
    }

    #[test]
    fn inbox_since_pages_forward_without_dropping_backlog() {
        let s = mem();
        let id1 = s.send("a", "b", None, "m1", None, None).unwrap();
        let id2 = s.send("a", "b", None, "m2", None, None).unwrap();
        let id3 = s.send("a", "all", None, "bcast", None, None).unwrap();

        let all = s.inbox_since("b", 0, 50).unwrap();
        let ids: Vec<i64> = all.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![id1, id2, id3]);

        let fwd = s.inbox_since("b", id1, 50).unwrap();
        let ids: Vec<i64> = fwd.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![id2, id3]);

        // Does not mark anything read.
        let (unread, _) = s.inbox("b", false, false, 50).unwrap();
        assert_eq!(unread.len(), 3);

        // Excludes the caller's own messages.
        assert!(s.inbox_since("a", 0, 50).unwrap().is_empty());
    }

    #[test]
    fn gc_deletes_old_keeps_new() {
        let s = mem();
        let id_old = s.send("a", "b", None, "old", None, None).unwrap();
        // Backdate the first message well past the threshold.
        s.rt.block_on(async {
            s.conn
                .execute(
                    "UPDATE messages SET ts = ts - 100000 WHERE id = ?1",
                    params(vec![id_old.into()]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap();
        s.send("a", "b", None, "new", None, None).unwrap();
        let deleted = s.gc(3600).unwrap(); // older than 1h
        assert_eq!(deleted, 1);
        assert_eq!(s.total_messages().unwrap(), 1);
        let (rows, _) = s.inbox("b", true, false, 50).unwrap();
        assert_eq!(rows[0].body, "new");
    }

    #[test]
    fn gc_clears_effective_supersession_link_to_deleted_successor_libsql() {
        let s = mem();
        let predecessor = s.send("a", "b", None, "first", None, None).unwrap();
        let successor = s.send("a", "b", None, "second", None, None).unwrap();
        s.supersede("a", predecessor, successor).unwrap();
        s.rt.block_on(async {
            s.conn
                .execute(
                    "UPDATE messages SET ts = ts - 100000 WHERE id = ?1",
                    params(vec![successor.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

        assert_eq!(s.gc(3600).unwrap(), 1);
        let link =
            s.rt.block_on(async {
                let mut rows = s
                    .conn
                    .query(
                        "SELECT superseded_by FROM messages WHERE id = ?1",
                        params(vec![predecessor.into()]),
                    )
                    .await?;
                let row = rows.next().await?.expect("predecessor remains");
                Ok::<_, anyhow::Error>(row.get::<Option<i64>>(0)?)
            })
            .unwrap();
        assert_eq!(link, None);
    }

    /// P6 (libsql parity): record_delivery appends metadata-only stages that
    /// list_delivery returns oldest-first; the body never appears in a trace.
    #[test]
    fn delivery_log_records_and_lists_oldest_first_libsql() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage};
        let s = mem();
        let mid = s
            .send("a", "b", None, "SECRET-BODY-XYZ", None, None)
            .unwrap();
        s.record_delivery(
            mid,
            DeliveryRefKind::Message.as_str(),
            "b",
            DeliveryStage::Queued.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        s.record_delivery(
            mid,
            DeliveryRefKind::Message.as_str(),
            "b",
            DeliveryStage::Drained.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        let trace = s.list_delivery(mid, 50).unwrap();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0].stage, "queued");
        assert_eq!(trace[1].stage, "drained");
        for t in &trace {
            assert!(!format!("{t:?}").contains("SECRET-BODY-XYZ"));
        }
        assert!(s.list_delivery(999_999, 50).unwrap().is_empty());
    }

    /// P6 (libsql parity): list_delivery is bounded by MAX_DELIVERY_ROWS.
    #[test]
    fn delivery_log_read_is_bounded_libsql() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage, MAX_DELIVERY_ROWS};
        let s = mem();
        let mid = s.send("a", "b", None, "x", None, None).unwrap();
        for _ in 0..(MAX_DELIVERY_ROWS + 10) {
            s.record_delivery(
                mid,
                DeliveryRefKind::Notify.as_str(),
                "b",
                DeliveryStage::Queued.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap();
        }
        assert_eq!(
            s.list_delivery(mid, i64::MAX).unwrap().len() as i64,
            MAX_DELIVERY_ROWS
        );
    }

    #[test]
    fn delivery_exists_searches_beyond_the_display_cap_libsql() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage, MAX_DELIVERY_ROWS};
        let s = mem();
        let mid = s.send("a", "b", None, "x", None, None).unwrap();
        for _ in 0..(MAX_DELIVERY_ROWS + 5) {
            s.record_delivery(
                mid,
                DeliveryRefKind::Message.as_str(),
                "telegram",
                DeliveryStage::RelayFailed.as_str(),
                DeliveryOutcome::Fail.as_str(),
            )
            .unwrap();
        }
        s.record_delivery(
            mid,
            DeliveryRefKind::Message.as_str(),
            "telegram",
            DeliveryStage::Relayed.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        assert!(!s
            .list_delivery(mid, i64::MAX)
            .unwrap()
            .iter()
            .any(|trace| trace.stage == DeliveryStage::Relayed.as_str()));
        assert!(s
            .has_delivery(
                mid,
                DeliveryRefKind::Message.as_str(),
                "telegram",
                DeliveryStage::Relayed.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap());
        assert!(!s
            .has_delivery(
                mid,
                DeliveryRefKind::Message.as_str(),
                "slack",
                DeliveryStage::Relayed.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap());
        assert!(!s
            .has_delivery(
                -1,
                DeliveryRefKind::Message.as_str(),
                "telegram",
                DeliveryStage::Relayed.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap());
    }

    /// P6 (libsql parity): gc prunes old delivery_log rows in the same pass.
    #[test]
    fn gc_prunes_old_delivery_log_libsql() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage};
        let s = mem();
        let mid = s.send("a", "b", None, "m", None, None).unwrap();
        s.record_delivery(
            mid,
            DeliveryRefKind::Message.as_str(),
            "b",
            DeliveryStage::Queued.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        // Backdate the trace row past the threshold.
        s.rt.block_on(async {
            s.conn
                .execute(
                    "UPDATE delivery_log SET ts = ts - 100000 WHERE ref_id = ?1",
                    params(vec![mid.into()]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap();
        s.record_delivery(
            mid,
            DeliveryRefKind::Message.as_str(),
            "b",
            DeliveryStage::Drained.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        s.gc(3600).unwrap();
        let trace = s.list_delivery(mid, 50).unwrap();
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].stage, "drained");
    }

    /// P6 (libsql OWNER-ONLY-WRITES): record_delivery traps on a read-only handle
    /// FIRST (write-trap parity); reads still work. Mirrors record_revocation.
    #[test]
    fn record_delivery_traps_on_readonly_handle_libsql() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage};
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-delivguard-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foreign.db");
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            rw.record_delivery(
                1,
                DeliveryRefKind::Message.as_str(),
                "b",
                DeliveryStage::Queued.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap();
        }
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(ro.read_only, "open_readonly sets the guard flag");
        let e = ro
            .record_delivery(
                2,
                DeliveryRefKind::Message.as_str(),
                "b",
                DeliveryStage::Injected.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .expect_err("record_delivery must trap on a read-only handle");
        assert!(
            e.to_string()
                .contains("BUG: write attempted on a read-only foreign store"),
            "wrong trap error: {e}"
        );
        // Reads still work on the RO handle.
        assert_eq!(ro.list_delivery(1, 50).unwrap().len(), 1);
    }

    #[test]
    fn negative_limit_is_not_unbounded() {
        let s = mem();
        for i in 0..5 {
            s.send("a", "b", None, &format!("m{i}"), None, None)
                .unwrap();
        }
        // A negative limit must NOT behave like SQLite's unbounded LIMIT -1.
        let (rows, _) = s.inbox("b", true, false, -1).unwrap();
        assert!(rows.len() <= MAX_LIMIT as usize);
        assert_eq!(rows.len(), 5);
        assert_eq!(clamp_limit(-1), MAX_LIMIT);
    }

    #[test]
    fn clear_inbox_and_clear_all() {
        let s = mem();
        s.send("a", "b", None, "1", None, None).unwrap();
        s.send("a", "b", None, "2", None, None).unwrap();
        // clear_inbox marks b's unread read (returns the count marked).
        assert_eq!(s.clear_inbox("b").unwrap(), 2);
        let (unread, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(unread.is_empty());
        // The messages still exist until clear_all wipes them.
        assert_eq!(s.total_messages().unwrap(), 2);
        assert_eq!(s.clear_all().unwrap(), 2);
        assert_eq!(s.total_messages().unwrap(), 0);
    }

    #[test]
    fn send_rejects_invalid_idents() {
        let s = mem();
        assert!(
            s.send("", "b", None, "x", None, None).is_err(),
            "empty sender rejected"
        );
        assert!(
            s.send("a", "", None, "x", None, None).is_err(),
            "empty recipient rejected"
        );
        assert!(
            s.send("a", "b\nc", None, "x", None, None).is_err(),
            "control char in recipient rejected"
        );
        assert!(
            s.send(
                "a",
                "b",
                Some(&"é".repeat(crate::store::MAX_SUBJECT_LEN + 1)),
                "x",
                None,
                None,
            )
            .is_err(),
            "subject cap is counted in Unicode scalar values"
        );
        assert!(
            s.send("a", "b", Some("bad\nsubject"), "x", None, None)
                .is_err(),
            "control characters in a subject are rejected"
        );
        let key = "explicit-empty-subject";
        s.send("a", "b", Some(""), "x", Some(key), None).unwrap();
        assert_eq!(
            s.message_by_idempotency_key(key)
                .unwrap()
                .unwrap()
                .subject
                .as_deref(),
            Some(""),
            "an explicit empty subject remains distinct from omission"
        );
        assert!(s.send("a", "b", None, "x", None, None).is_ok());
    }

    // ---- A2 (real liveness): mirror of the SqliteStore store-unit tests ----

    /// `register_peer_full` round-trips the new `pid`/`host` columns through both
    /// `get_peer` and `list_peers`, and an upsert overwrites them. (libSQL mirror.)
    #[test]
    fn register_peer_full_roundtrips_pid_and_host() {
        let s = mem();
        let cert = s
            .register_peer_full(
                "p",
                "tmux",
                "%3",
                "",
                Some("/w"),
                Some(4321),
                "boxA",
                "weave",
                "main",
                "(main)",
                "default",
                None,
                "",
            )
            .unwrap();
        let p = s.get_peer("p").unwrap().unwrap();
        assert_eq!(p.pid, Some(4321));
        assert_eq!(p.repo, "weave");
        assert_eq!(p.branch, "main");
        assert_eq!(p.worktree_id, "(main)");
        assert_eq!(p.host, "boxA");
        let lp = &s.list_peers().unwrap()[0];
        assert_eq!(lp.pid, Some(4321));
        assert_eq!(lp.host, "boxA");
        // Upsert overwrites pid/host (and a None pid clears it).
        s.register_peer_full(
            "p",
            "tmux",
            "%3",
            "",
            Some("/w"),
            None,
            "boxB",
            "",
            "",
            "",
            "default",
            Some(&cert),
            "",
        )
        .unwrap();
        let p2 = s.get_peer("p").unwrap().unwrap();
        assert_eq!(p2.pid, None);
        assert_eq!(p2.host, "boxB");
        // The 5-arg compat wrapper forwards pid=None, host="".
        s.register_peer("q", "none", "", "", None).unwrap();
        let q = s.get_peer("q").unwrap().unwrap();
        assert_eq!(q.pid, None);
        assert_eq!(q.host, "");
    }

    /// A libSQL DB created WITHOUT the `pid`/`host` columns opens and gains them in
    /// place: the inline migration adds them, the legacy row survives, and reads
    /// back `pid:None`/`host:""`. (libSQL mirror of the sqlite legacy-migration
    /// test.) The pre-A2 table is built directly with libsql's own Builder so the
    /// store's `open` exercises the real ADD COLUMN path.
    #[test]
    fn legacy_db_without_pid_host_migrates_in_place() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-legacy-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // Build a pre-A2 peers table (no pid/host) + a legacy row, via a raw libsql
        // connection, then close it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "CREATE TABLE peers (
                    name      TEXT PRIMARY KEY,
                    mux       TEXT NOT NULL,
                    target    TEXT NOT NULL,
                    socket    TEXT NOT NULL DEFAULT '',
                    cwd       TEXT,
                    last_seen INTEGER NOT NULL
                 )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1)",
                params(vec![now().into()]),
            )
            .await
            .unwrap();
        });
        drop(rt);

        // Opening through LibsqlStore runs the inline migration: the columns appear.
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let s = LibsqlStore::open(&cfg).unwrap();
        let p = s.get_peer("old").unwrap().unwrap();
        assert_eq!(p.mux, "tmux");
        assert_eq!(p.target, "%1");
        assert_eq!(p.cwd.as_deref(), Some("/legacy"));
        assert_eq!(p.pid, None, "legacy row reads pid:None after migration");
        assert_eq!(p.host, "", "legacy row reads host:'' after migration");
        // Re-opening is idempotent and a fresh full register works on the upgrade.
        let s2 = LibsqlStore::open(&cfg).unwrap();
        s2.register_peer_full(
            "new",
            "tmux",
            "%2",
            "",
            None,
            Some(7),
            "h",
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let nrow = s2.get_peer("new").unwrap().unwrap();
        assert_eq!(nrow.pid, Some(7));
        assert_eq!(nrow.host, "h");
    }

    /// P4 (libSQL mirror): a pre-P4 peers table (no circle/role) migrates in place;
    /// a legacy row reads circle='default'/role='peer'; re-open is a no-op.
    #[test]
    fn legacy_db_without_circle_role_migrates_in_place() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-circle-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy-circle.db");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            // A post-tags but pre-P4 peers table.
            conn.execute(
                "CREATE TABLE peers (
                    name        TEXT PRIMARY KEY,
                    mux         TEXT NOT NULL,
                    target      TEXT NOT NULL,
                    socket      TEXT NOT NULL DEFAULT '',
                    cwd         TEXT,
                    last_seen   INTEGER NOT NULL,
                    pid         INTEGER,
                    host        TEXT NOT NULL DEFAULT '',
                    repo        TEXT NOT NULL DEFAULT '',
                    branch      TEXT NOT NULL DEFAULT '',
                    worktree_id TEXT NOT NULL DEFAULT ''
                 )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1, 42, 'h', '', '', '')",
                params(vec![now().into()]),
            )
            .await
            .unwrap();
        });
        drop(rt);
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let s = LibsqlStore::open(&cfg).unwrap();
        let old = s.get_peer("old").unwrap().unwrap();
        assert_eq!(old.circle, "default");
        assert_eq!(old.role, "peer");
        // Re-open is idempotent.
        let s2 = LibsqlStore::open(&cfg).unwrap();
        assert!(s2.get_peer("old").unwrap().is_some());
    }

    /// P4 (libSQL mirror): register round-trips the circle and a re-register
    /// preserves an orchestrator role.
    #[test]
    fn register_roundtrips_circle_and_preserves_role() {
        let s = mem();
        let cert = s
            .register_peer_full(
                "p", "tmux", "%1", "", None, None, "h", "", "", "", "team-a", None, "",
            )
            .unwrap();
        assert_eq!(s.get_peer("p").unwrap().unwrap().circle, "team-a");
        s.claim_orchestrator_role("p", None, false).unwrap();
        assert_eq!(s.get_peer("p").unwrap().unwrap().role, "orchestrator");
        s.register_peer_full(
            "p",
            "tmux",
            "%1",
            "",
            None,
            None,
            "h",
            "",
            "",
            "",
            "team-a",
            Some(&cert),
            "",
        )
        .unwrap();
        assert_eq!(
            s.get_peer("p").unwrap().unwrap().role,
            "orchestrator",
            "a re-register must not demote an orchestrator"
        );
    }

    /// P4 (libSQL mirror): claim refuses a non-force claim while a LIVE holder
    /// exists, and force steals (demoting the prior holder).
    #[test]
    fn claim_co_orchestrator_and_force_steals() {
        let s = mem();
        s.register_peer_full(
            "a", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None, "",
        )
        .unwrap();
        s.register_peer_full(
            "b", "tmux", "%2", "", None, None, "h", "", "", "", "c1", None, "",
        )
        .unwrap();
        assert!(matches!(
            s.claim_orchestrator_role("a", None, false).unwrap(),
            crate::model::ClaimOutcome::Claimed { .. }
        ));
        // WL-019: b claims without force while a is live ⇒ co-orchestrator.
        match s.claim_orchestrator_role("b", None, false).unwrap() {
            crate::model::ClaimOutcome::Claimed { demoted, .. } => {
                assert!(
                    demoted.is_empty(),
                    "non-force should not demote: {demoted:?}"
                );
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        assert_eq!(s.get_peer("a").unwrap().unwrap().role, "orchestrator");
        assert_eq!(s.get_peer("b").unwrap().unwrap().role, "orchestrator");
        match s.claim_orchestrator_role("b", None, true).unwrap() {
            crate::model::ClaimOutcome::Claimed { demoted, .. } => {
                assert_eq!(demoted, vec!["a".to_string()])
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        assert_eq!(s.get_peer("a").unwrap().unwrap().role, "peer");
        assert_eq!(s.get_peer("b").unwrap().unwrap().role, "orchestrator");
        assert!(s.claim_orchestrator_role("ghost", None, false).is_err());
    }

    /// P4 (libSQL mirror): `orchestrator_status` reuses `is_alive` — a fresh holder
    /// reads present; an empty circle reads absent.
    #[test]
    fn orchestrator_status_present_and_absent() {
        let s = mem();
        s.register_peer_full(
            "o", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None, "",
        )
        .unwrap();
        s.claim_orchestrator_role("o", None, false).unwrap();
        let st = s.orchestrator_status(Some("c1")).unwrap();
        assert!(st.present);
        assert_eq!(st.holders[0].name, "o");
        let st2 = s.orchestrator_status(Some("empty")).unwrap();
        assert!(!st2.present);
    }

    /// A libSQL DB whose `peers` table predates the session-tag columns (no `repo`,
    /// `branch`, `worktree_id`) opens NON-FATALLY: the inline migration adds the
    /// three columns, the legacy row survives reading empty tags, and a peer
    /// registered with tags roundtrips through `get_peer`/`list_peers`. (libSQL
    /// mirror of the sqlite `legacy_db_without_git_tag_columns_migrates_and_roundtrips`.)
    #[test]
    fn legacy_db_without_git_tag_columns_migrates_and_roundtrips() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-legacy-tags-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy-tags.db");

        // Build a peers table with pid/host but WITHOUT the three tag columns +
        // a legacy row, via a raw libsql connection.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "CREATE TABLE peers (
                    name      TEXT PRIMARY KEY,
                    mux       TEXT NOT NULL,
                    target    TEXT NOT NULL,
                    socket    TEXT NOT NULL DEFAULT '',
                    cwd       TEXT,
                    last_seen INTEGER NOT NULL,
                    pid       INTEGER,
                    host      TEXT NOT NULL DEFAULT ''
                 )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1, 42, 'h')",
                params(vec![now().into()]),
            )
            .await
            .unwrap();
        });
        drop(rt);

        // Opening runs the inline migration: the 3 tag columns are added.
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let s = LibsqlStore::open(&cfg).unwrap();
        let old = s.get_peer("old").unwrap().unwrap();
        assert_eq!(old.pid, Some(42));
        assert_eq!(old.host, "h");
        assert_eq!(
            (
                old.repo.as_str(),
                old.branch.as_str(),
                old.worktree_id.as_str()
            ),
            ("", "", ""),
            "legacy libSQL row reads empty tags after migration"
        );

        // A peer registered with tags roundtrips at the projection positions.
        s.register_peer_full(
            "tagged",
            "tmux",
            "%2",
            "",
            Some("/w"),
            Some(7),
            "h2",
            "weave",
            "feat/x",
            "wt-9",
            "default",
            None,
            "",
        )
        .unwrap();
        let g = s.get_peer("tagged").unwrap().unwrap();
        assert_eq!(
            (g.repo.as_str(), g.branch.as_str(), g.worktree_id.as_str()),
            ("weave", "feat/x", "wt-9"),
            "get_peer roundtrips the migrated libSQL tag columns"
        );
        let lp = s
            .list_peers()
            .unwrap()
            .into_iter()
            .find(|p| p.name == "tagged")
            .unwrap();
        assert_eq!(
            (
                lp.repo.as_str(),
                lp.branch.as_str(),
                lp.worktree_id.as_str()
            ),
            ("weave", "feat/x", "wt-9"),
            "list_peers roundtrips the migrated libSQL tag columns"
        );
        // Re-opening is idempotent.
        assert!(LibsqlStore::open(&cfg)
            .unwrap()
            .get_peer("old")
            .unwrap()
            .is_some());
    }

    /// `is_alive` matrix against the libSQL backend's actual rows: a remote-host
    /// peer must read alive (fail-open) so the Turso/shared-DB case can't read every
    /// peer dead; a local dead-pid peer reads offline on Linux; a NULL-pid peer
    /// falls back to TTL. (Mirror of the sqlite is_alive matrix, driven through real
    /// libSQL persistence + read-back.)
    #[test]
    fn is_alive_matrix_against_libsql_rows() {
        let s = mem();

        // (c) NULL pid + recent => alive (TTL fallback).
        s.register_peer_full(
            "nullpid",
            "tmux",
            "%1",
            "",
            None,
            None,
            &crate::config::this_host(),
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let nullpid = s.get_peer("nullpid").unwrap().unwrap();
        assert!(is_alive(&nullpid), "null pid + recent must be alive (TTL)");

        // (b) remote host (host != this_host) + recent + absurd pid => alive
        //     (fail-open: cannot probe a remote PID — the Turso case).
        let remote_host = format!("{}-remote", crate::config::this_host());
        s.register_peer_full(
            "remote",
            "tmux",
            "%2",
            "",
            None,
            Some(999_999_999),
            &remote_host,
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let remote = s.get_peer("remote").unwrap().unwrap();
        assert_ne!(remote.host, crate::config::this_host());
        assert!(
            is_alive(&remote),
            "remote-host peer must fail open to alive (Turso shared-DB case)"
        );

        // (d) local host + our OWN live pid => alive.
        s.register_peer_full(
            "live",
            "tmux",
            "%3",
            "",
            None,
            Some(std::process::id() as i64),
            &crate::config::this_host(),
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let live = s.get_peer("live").unwrap().unwrap();
        assert!(
            is_alive(&live),
            "local host + our own live pid must be alive"
        );

        // (a) local host + dead pid => offline on Linux (probe is real).
        s.register_peer_full(
            "dead",
            "tmux",
            "%4",
            "",
            None,
            Some(999_999_999),
            &crate::config::this_host(),
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let dead = s.get_peer("dead").unwrap().unwrap();
        if cfg!(target_os = "linux") {
            assert!(
                !is_alive(&dead),
                "local host + dead pid must read offline under A2 (libsql)"
            );
        }

        // Recency guard: backdate the live-pid peer past the TTL => offline.
        s.backdate_peer("live", ONLINE_TTL_SECS + 1).unwrap();
        let stale = s.get_peer("live").unwrap().unwrap();
        assert!(
            !is_alive(&stale),
            "stale last_seen is offline even with a live pid (libsql)"
        );
    }

    /// Host-aware `liveness_for` classifier against real libSQL rows (mirror of
    /// the sqlite `liveness_for_matrix_*` unit test). FIXED `this_host` + `now_ts`
    /// (no real hostname/clock) except the same-host pid probe.
    #[test]
    fn liveness_for_matrix_against_libsql_rows() {
        let s = mem();
        let this = crate::config::this_host();
        let now_ts = crate::model::now();

        // same-host + our OWN live pid => AliveLocal.
        s.register_peer_full(
            "local",
            "tmux",
            "%1",
            "",
            None,
            Some(std::process::id() as i64),
            &this,
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let local = s.get_peer("local").unwrap().unwrap();
        assert_eq!(liveness_for(&local, &this, now_ts), Liveness::AliveLocal);

        // same-host + null pid + recent => AliveLocal (TTL fallback).
        s.register_peer_full(
            "nullpid", "tmux", "%2", "", None, None, &this, "", "", "", "default", None, "",
        )
        .unwrap();
        let nullpid = s.get_peer("nullpid").unwrap().unwrap();
        assert_eq!(liveness_for(&nullpid, &this, now_ts), Liveness::AliveLocal);

        // remote host + recent + absurd pid => AliveRemote (NEVER probed).
        let remote_host = format!("{this}-remote");
        s.register_peer_full(
            "remote",
            "tmux",
            "%3",
            "",
            None,
            Some(999_999_999),
            &remote_host,
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let remote = s.get_peer("remote").unwrap().unwrap();
        assert_eq!(liveness_for(&remote, &this, now_ts), Liveness::AliveRemote);

        // empty host + recent => AliveRemote (fail-open).
        s.register_peer_full(
            "empty",
            "tmux",
            "%4",
            "",
            None,
            Some(999_999_999),
            "",
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let empty = s.get_peer("empty").unwrap().unwrap();
        assert_eq!(liveness_for(&empty, &this, now_ts), Liveness::AliveRemote);

        // remote host backdated past the TTL => Stale.
        s.backdate_peer("remote", ONLINE_TTL_SECS + 1).unwrap();
        let remote_stale = s.get_peer("remote").unwrap().unwrap();
        assert_eq!(
            liveness_for(&remote_stale, &this, crate::model::now()),
            Liveness::Stale
        );

        // same-host dead pid + recent => Stale on Linux (real /proc probe).
        s.register_peer_full(
            "dead",
            "tmux",
            "%5",
            "",
            None,
            Some(999_999_999),
            &this,
            "",
            "",
            "",
            "default",
            None,
            "",
        )
        .unwrap();
        let dead = s.get_peer("dead").unwrap().unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(
                liveness_for(&dead, &this, crate::model::now()),
                Liveness::Stale
            );
        }

        // Delegation regression-lock: (liveness_for != Stale) == is_alive.
        for name in ["local", "nullpid", "empty", "dead"] {
            let p = s.get_peer(name).unwrap().unwrap();
            assert_eq!(
                liveness_for(&p, &crate::config::this_host(), crate::model::now())
                    != Liveness::Stale,
                is_alive(&p),
                "is_alive must equal (liveness_for != Stale) for {name}"
            );
        }
    }

    /// `pid_alive` (shared pure helper) behaves the same when exercised from the
    /// libSQL build: our own pid is alive; an absurd pid / pid<=0 is dead on Linux,
    /// degraded-alive elsewhere. Mirrors the sqlite unit so both build feature sets
    /// cover the probe contract.
    #[test]
    fn pid_alive_own_pid_live_absurd_pid_dead() {
        let me = std::process::id() as i64;
        assert!(pid_alive(me), "our own pid must be alive");
        if cfg!(target_os = "linux") {
            assert!(
                !pid_alive(999_999_999),
                "an unused pid is not alive (linux)"
            );
            assert!(!pid_alive(0), "pid 0 is rejected");
            assert!(!pid_alive(-1), "a negative pid is rejected");
        } else {
            assert!(pid_alive(999_999_999), "non-linux degrades to assume-alive");
        }
    }

    // ---- Tier-1 federation: libsql read-only open is structurally write-incapable ----

    /// The structural read-only proof on the LIBSQL backend (mirror of the sqlite
    /// `open_readonly_reads_but_cannot_write`): `open_readonly` can READ an existing
    /// store but the libsql/SQLite core rejects every write, leaves the foreign file
    /// byte-identical (sha-free content compare), and never creates a missing file.
    #[test]
    fn open_readonly_reads_but_cannot_write_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-ro-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.db");

        // Seed a store with a peer via the normal RW open, then drop it.
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            rw.register_peer_full(
                "seed",
                "tmux",
                "%1",
                "",
                Some("/w"),
                Some(7),
                "boxA",
                "",
                "",
                "",
                "default",
                None,
                "",
            )
            .unwrap();
        }

        // Capture the exact main-DB-file bytes BEFORE any federated read. (A
        // WAL-mode store legitimately materializes an empty `-shm` on a read-only
        // open — documented SQLite behavior, not a data write — so the invariant
        // is asserted on the main DATA file plus an empty/absent WAL.)
        let before = std::fs::read(&path).expect("read foreign main DB bytes (before)");

        // Read-only open can list the peer.
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        let peers = ro.list_peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "seed");

        // But ANY write is rejected by the engine, not by convention.
        let wr = ro.register_peer_full(
            "intruder", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None, "",
        );
        assert!(
            wr.is_err(),
            "a write through a libsql read-only handle must error"
        );
        let send = ro.send("a", "b", None, "x", None, None);
        assert!(
            send.is_err(),
            "a send through a libsql read-only handle must error"
        );
        // Reading the same handle again still succeeds (the failed writes were no-ops).
        assert_eq!(ro.list_peers().unwrap().len(), 1);
        drop(ro);

        // The main DB file is byte-identical: no row touched, no migration. And no
        // write was committed — the WAL is empty/absent.
        let after = std::fs::read(&path).expect("read foreign main DB bytes (after)");
        assert_eq!(
            before, after,
            "a federated read-only open must leave the foreign libsql main DB byte-unchanged"
        );
        let wal_len = std::fs::metadata(format!("{}-wal", path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            wal_len, 0,
            "a read-only libsql open must not commit a write (WAL empty/absent)"
        );

        // Opening a path that does not exist read-only must NOT create it.
        let missing = dir.join("does-not-exist.db");
        assert!(LibsqlStore::open_readonly(&missing).is_err());
        assert!(
            !missing.exists(),
            "read-only libsql open must never create a missing store"
        );
    }

    /// OWNER-ONLY-WRITES (the Tier-2 v2 crux), hermetic / NO network: a `read_only`-
    /// flagged `LibsqlStore` (the same flag set on every remote/foreign open) makes
    /// EVERY write method return the `guard_writable` `bail!` error — never a panic,
    /// never a write — and the foreign DATA file is byte-identical afterwards. We use
    /// a LOCAL-file read-only handle (`open_readonly`, which sets `read_only = true`,
    /// the identical flag `open_readonly_remote` sets) so the proof needs no live
    /// Turso. This is the remote analogue of the sqlite "foreign store byte-unchanged"
    /// assertion, proving the guard without a network connection.
    #[test]
    fn read_only_handle_traps_every_write_and_leaves_file_unchanged() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-guard-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foreign.db");

        // Seed a foreign store, then drop the writer so all WAL is flushed.
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            rw.register_peer_full(
                "seed",
                "tmux",
                "%1",
                "",
                Some("/w"),
                Some(7),
                "boxA",
                "",
                "",
                "",
                "default",
                None,
                "",
            )
            .unwrap();
            rw.send("seed", "seed", None, "hi", None, None).unwrap();
        }
        let before = std::fs::read(&path).expect("read foreign DB bytes (before)");

        // The read-only-flagged handle (the exact flag a remote open sets).
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(
            ro.read_only,
            "open_readonly must set the read-only guard flag"
        );

        // Every write method must return the guard error (NOT panic, NOT write).
        let guarded = "BUG: write attempted on a read-only foreign store";
        let assert_trapped = |what: &str, r: Result<()>| {
            let e = r.expect_err(&format!("{what} must be trapped on a read-only handle"));
            assert!(
                e.to_string().contains(guarded),
                "{what} returned the wrong error: {e}"
            );
        };

        assert_trapped("send", ro.send("a", "b", None, "x", None, None).map(|_| ()));
        assert_trapped(
            "inbox(mark_read)",
            ro.inbox("seed", false, true, 10).map(|_| ()),
        );
        assert_trapped("clear_inbox", ro.clear_inbox("seed").map(|_| ()));
        assert_trapped("clear_all", ro.clear_all().map(|_| ()));
        assert_trapped("gc", ro.gc(0).map(|_| ()));
        assert_trapped("set_in_reply_to", ro.set_in_reply_to(1, 1));
        assert_trapped("reply", ro.reply("a", 1, "x").map(|_| ()));
        assert_trapped("touch_peer", ro.touch_peer("seed"));
        assert_trapped(
            "register_peer_full",
            ro.register_peer_full(
                "intruder", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None, "",
            )
            .map(|_| ()),
        );
        assert_trapped(
            "enqueue_intent",
            ro.enqueue_intent("to", "boxB", "from", None, "body", "", None, None, None, 0)
                .map(|_| ()),
        );
        assert_trapped("pull_cursor_set", ro.pull_cursor_set("src", 5));
        assert_trapped("register_key", ro.register_key("id", "pubkey"));
        assert_trapped("store_summary", ro.store_summary(1, "summary", "model"));
        assert_trapped(
            "store_summary_if_generation",
            ro.store_summary_if_generation(1, "summary", "model", 0)
                .map(|_| ()),
        );
        assert_trapped("delete_summary", ro.delete_summary(1).map(|_| ()));

        // A read still works (the failed writes were no-ops).
        assert_eq!(ro.list_peers().unwrap().len(), 1);
        drop(ro);

        // The foreign DATA file is byte-identical; the WAL committed nothing.
        let after = std::fs::read(&path).expect("read foreign DB bytes (after)");
        assert_eq!(
            before, after,
            "a read-only foreign handle must leave the source DB byte-unchanged"
        );
        let wal_len = std::fs::metadata(format!("{}-wal", path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "no write may be committed to the foreign WAL");
    }

    /// `federated_peers` (libsql) unions the local peers with a foreign read-only
    /// store, origin-tagging foreign rows; an unreadable extra store is skipped (the
    /// local listing still returns) — mirrors the sqlite federation aggregation test.
    #[test]
    fn federated_peers_unions_and_isolates_failures_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-fed-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let local_path = dir.join("local.db");
        let foreign_path = dir.join("foreign.db");

        let local = {
            let cfg = Config {
                db: Some(local_path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            LibsqlStore::open(&cfg).unwrap()
        };
        local
            .register_peer_full(
                "me", "tmux", "%1", "", None, None, "boxA", "", "", "", "default", None, "",
            )
            .unwrap();
        {
            let cfg = Config {
                db: Some(foreign_path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let foreign = LibsqlStore::open(&cfg).unwrap();
            foreign
                .register_peer_full(
                    "them", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None, "",
                )
                .unwrap();
        }

        // A bad path is skipped, not fatal.
        let bad = dir.join("nope.db");
        let extra = vec![
            StoreSource::Local(foreign_path.clone()),
            StoreSource::Local(bad),
        ];
        let views = federated_peers(&local, &extra).unwrap();
        let names: Vec<&str> = views.iter().map(|v| v.peer.name.as_str()).collect();
        assert!(names.contains(&"me"));
        assert!(names.contains(&"them"));
        let them = views.iter().find(|v| v.peer.name == "them").unwrap();
        assert!(them.origin.is_foreign());
        let me = views.iter().find(|v| v.peer.name == "me").unwrap();
        assert_eq!(me.origin, Origin::Local);

        // federation_status counts the ok store and the skipped bad path.
        let (ok, skipped) = federation_status(&extra);
        assert_eq!((ok, skipped), (1, 1));
    }

    // ---- Tier-2: outbox / pull cursor / pull driver (libsql mirror) ----

    /// `enqueue_intent` round-trips every column (incl. reserved empty `sig`), and
    /// `list_outbox` filters by recipient + `id>since`, oldest-first. (libsql.)
    #[test]
    fn enqueue_and_list_outbox_roundtrip_libsql() {
        let s = mem();
        let i1 = s
            .enqueue_intent(
                "bob",
                "boxB",
                "alice",
                Some("hi"),
                "body1",
                "",
                None,
                None,
                None,
                0,
            )
            .unwrap();
        s.enqueue_intent(
            "carol",
            "",
            "alice",
            None,
            "for carol",
            "",
            None,
            None,
            None,
            0,
        )
        .unwrap();
        let i3 = s
            .enqueue_intent("bob", "", "alice", None, "body3", "", None, None, None, 0)
            .unwrap();

        let all = s.outbox_all(50).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].to, "bob");
        assert_eq!(all[0].to_host, "boxB");
        assert_eq!(all[0].subject.as_deref(), Some("hi"));
        assert_eq!(all[0].sig, "");

        let for_bob = s.list_outbox("bob", 0, 50).unwrap();
        assert_eq!(
            for_bob.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![i1, i3]
        );
        let after = s.list_outbox("bob", i1, 50).unwrap();
        assert_eq!(after.iter().map(|m| m.id).collect::<Vec<_>>(), vec![i3]);
    }

    #[test]
    fn outbox_request_priority_normalizes_before_replay_libsql() {
        let s = mem();
        let (urgent_id, urgent_created) = s
            .enqueue_intent_idempotent(
                "bob",
                "",
                "alice",
                None,
                "urgent body",
                "",
                Some("event:outbox-priority-urgent"),
                None,
                Some("URGENT"),
                60,
            )
            .unwrap();
        assert!(urgent_created);
        assert_eq!(
            s.enqueue_intent_idempotent(
                "bob",
                "",
                "alice",
                None,
                "urgent body",
                "",
                Some("event:outbox-priority-urgent"),
                None,
                Some("urgent"),
                60,
            )
            .unwrap(),
            (urgent_id, false)
        );

        let (normal_id, normal_created) = s
            .enqueue_intent_idempotent(
                "bob",
                "",
                "alice",
                None,
                "normal body",
                "",
                Some("event:outbox-priority-default"),
                None,
                Some("not-a-priority"),
                0,
            )
            .unwrap();
        assert!(normal_created);
        assert_eq!(
            s.enqueue_intent_idempotent(
                "bob",
                "",
                "alice",
                None,
                "normal body",
                "",
                Some("event:outbox-priority-default"),
                None,
                Some("normal"),
                0,
            )
            .unwrap(),
            (normal_id, false)
        );

        let rows = s.outbox_all(10).unwrap();
        let urgent = rows.iter().find(|intent| intent.id == urgent_id).unwrap();
        assert_eq!(urgent.priority, "urgent");
        assert_eq!(urgent.ttl, 60);
        let normal = rows.iter().find(|intent| intent.id == normal_id).unwrap();
        assert_eq!(normal.priority, "normal");
        assert_eq!(normal.ttl, 0);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn outbox_request_invalid_ttl_is_atomic_libsql() {
        let s = mem();
        for (key, ttl) in [
            ("event:outbox-ttl-negative", -1),
            (
                "event:outbox-ttl-oversized",
                crate::model::MAX_MSG_TTL_SECS + 1,
            ),
        ] {
            assert!(s
                .enqueue_intent_idempotent(
                    "bob",
                    "",
                    "alice",
                    None,
                    "must not persist",
                    "",
                    Some(key),
                    None,
                    Some("urgent"),
                    ttl,
                )
                .is_err());
        }
        assert!(s.outbox_all(10).unwrap().is_empty());
        assert!(s.list_outbox("bob", 0, 10).unwrap().is_empty());
    }

    /// The pull cursor defaults to 0 and round-trips through set/get. (libsql.)
    #[test]
    fn pull_cursor_default_and_roundtrip_libsql() {
        let s = mem();
        assert_eq!(s.pull_cursor_get("/src.db").unwrap(), 0);
        s.pull_cursor_set("/src.db", 42).unwrap();
        assert_eq!(s.pull_cursor_get("/src.db").unwrap(), 42);
        s.pull_cursor_set("/src.db", 99).unwrap();
        assert_eq!(s.pull_cursor_get("/src.db").unwrap(), 99);
        assert_eq!(s.pull_cursor_get("/other.db").unwrap(), 0);
        s.pull_cursor_set("/src.db", 7).unwrap();
        assert_eq!(
            s.pull_cursor_get("/src.db").unwrap(),
            99,
            "a stale writer cannot regress the high-water mark"
        );
    }

    #[test]
    fn pull_cursors_are_scoped_by_recipient_and_host_libsql() {
        let _env = crate::testenv::lock_env();
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-pull-scope-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");
        let cfg = |path: &std::path::Path| Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        {
            let a = LibsqlStore::open(&cfg(&a_path)).unwrap();
            for (to, host, body) in [
                ("carol", "", "for carol"),
                ("bob", "", "for bob"),
                ("dave", "host-good", "for host-good"),
            ] {
                a.enqueue_intent(to, host, "alice", None, body, "", None, None, None, 0)
                    .unwrap();
            }
        }
        let b = LibsqlStore::open(&cfg(&b_path)).unwrap();
        let allow = vec![StoreSource::Local(a_path)];
        let wrong_host = crate::testenv::EnvVarGuard::set("HOSTNAME", "host-wrong");

        assert_eq!(
            pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            1
        );
        assert_eq!(
            pull_from_store(&b, "carol", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            1
        );
        assert_eq!(
            pull_from_store(&b, "dave", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            0
        );
        drop(wrong_host);
        let _right_host = crate::testenv::EnvVarGuard::set("HOSTNAME", "host-good");
        assert_eq!(
            pull_from_store(&b, "dave", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            1
        );
        assert_eq!(b.inbox("bob", false, false, 10).unwrap().0.len(), 1);
        assert_eq!(b.inbox("carol", false, false, 10).unwrap().0.len(), 1);
        assert_eq!(b.inbox("dave", false, false, 10).unwrap().0.len(), 1);
    }

    #[test]
    fn legacy_v1_cursor_migration_spans_bounded_drains_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let _env = crate::testenv::lock_env();
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-legacy-cursor-libsql-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.db");
        let local_path = dir.join("local.db");
        let cfg = |path: &std::path::Path| Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let keyed_ids = [64_i64, 128, 192, 256, 280, 300, 302];

        {
            let source_store = LibsqlStore::open(&cfg(&source_path)).unwrap();
            for expected_id in 1_i64..=302 {
                let key = keyed_ids
                    .contains(&expected_id)
                    .then(|| format!("event:legacy-cursor-{expected_id}"));
                let (id, created) = source_store
                    .enqueue_intent_idempotent(
                        "bob",
                        "",
                        "alice",
                        None,
                        &format!("intent-{expected_id}"),
                        "",
                        key.as_deref(),
                        None,
                        Some("normal"),
                        0,
                    )
                    .unwrap();
                assert_eq!(id, expected_id);
                assert!(created);
            }
        }

        let local = LibsqlStore::open(&cfg(&local_path)).unwrap();
        local
            .send("alice", "bob", None, "intent-1", None, None)
            .unwrap();
        local
            .send_configured_idempotent(
                "alice",
                "bob",
                None,
                "intent-64",
                Some("event:legacy-cursor-64"),
                None,
                Some("normal"),
                None,
                0,
                false,
            )
            .unwrap();

        let source = canonical_source(&source_path);
        let host = crate::config::this_host();
        let scoped_cursor = crate::store::pull_cursor_scope_key(&source, "bob", &host);
        local.pull_cursor_set(&source, 300).unwrap();
        let allow = vec![StoreSource::Local(source_path)];

        let first = pull_from_store(&local, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(first.committed, 3);
        assert_eq!(
            local.pull_cursor_get(&scoped_cursor).unwrap(),
            MAX_PULL_PER_DRAIN
        );

        let second = pull_from_store(&local, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(second.committed, 4);
        assert_eq!(local.pull_cursor_get(&scoped_cursor).unwrap(), 302);
        assert!(local.pull_cursor_get(&scoped_cursor).unwrap() > 300);

        let third = pull_from_store(&local, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(third.committed, 0);
        assert_eq!(local.pull_cursor_get(&scoped_cursor).unwrap(), 302);

        let messages = local.all_messages(100).unwrap();
        assert_eq!(messages.len(), 9);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.body == "intent-1")
                .count(),
            1,
            "the already-delivered legacy keyless row must not duplicate"
        );
        assert!(
            !messages.iter().any(|message| message.body == "intent-2"),
            "first-page legacy keyless rows stay skipped"
        );
        assert!(
            !messages.iter().any(|message| message.body == "intent-257"),
            "migration mode must remain active on the second bounded drain"
        );
        assert!(messages.iter().any(|message| message.body == "intent-301"));
        assert!(messages.iter().any(|message| message.body == "intent-302"));
        assert!(local
            .message_by_idempotency_key("event:legacy-cursor-302")
            .unwrap()
            .is_some());
        drop(local);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The `identity_keys` registry round-trips registered pubkeys through
    /// get/get_keys/list with ADD semantics + remove (libsql mirror of the sqlite
    /// `keys_register_get_list_roundtrip`). Plain data; present regardless of the
    /// `sign` feature.
    #[test]
    fn keys_register_get_list_roundtrip_libsql() {
        let s = mem();
        assert!(s.get_key("alice").unwrap().is_none());
        assert!(s.get_keys("alice").unwrap().is_empty());
        s.register_key("alice", "aa11").unwrap();
        s.register_key("bob", "bb22").unwrap();
        assert_eq!(s.get_keys("alice").unwrap(), vec!["aa11".to_string()]);
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));

        // ADD: a NEW key APPENDS (does NOT overwrite); both are registered.
        s.register_key("alice", "cc33").unwrap();
        assert_eq!(
            s.get_keys("alice").unwrap(),
            vec!["aa11".to_string(), "cc33".to_string()]
        );
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("cc33"));

        // Re-adding the SAME key is a NO-OP.
        s.register_key("alice", "aa11").unwrap();
        assert_eq!(s.get_keys("alice").unwrap().len(), 2);

        let keys = s.list_keys().unwrap();
        assert_eq!(
            keys,
            vec![
                ("alice".to_string(), "aa11".to_string()),
                ("alice".to_string(), "cc33".to_string()),
                ("bob".to_string(), "bb22".to_string()),
            ]
        );

        // remove_key removes exactly that pair; absent ⇒ false.
        assert!(s.remove_key("alice", "aa11").unwrap());
        assert_eq!(s.get_keys("alice").unwrap(), vec!["cc33".to_string()]);
        assert!(!s.remove_key("alice", "aa11").unwrap());

        assert!(s.register_key("", "00").is_err());
    }

    /// `MAX_KEYS_PER_IDENT` cap enforced on libsql (mirror): a duplicate is a no-op
    /// and never counts; the cap-th+1 DISTINCT key errors (never panics).
    #[test]
    fn register_key_enforces_per_identity_cap_libsql() {
        let s = mem();
        for i in 0..MAX_KEYS_PER_IDENT {
            let pk = format!("{:064x}", i);
            s.register_key("alice", &pk).unwrap();
        }
        assert_eq!(s.get_keys("alice").unwrap().len(), MAX_KEYS_PER_IDENT);
        let dup = format!("{:064x}", 0);
        s.register_key("alice", &dup).unwrap();
        assert_eq!(s.get_keys("alice").unwrap().len(), MAX_KEYS_PER_IDENT);
        let over = format!("{:064x}", MAX_KEYS_PER_IDENT + 1);
        assert!(s.register_key("alice", &over).is_err());
    }

    /// #7 migration on libsql: a legacy DB with a single-key `keys` row migrates
    /// that row into `identity_keys` on open; the copy is idempotent across opens.
    #[test]
    fn legacy_single_key_migrates_into_identity_keys_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-mklegacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // Build a pre-#7 DB: a `keys` table with one row, NO identity_keys table,
        // via a raw libsql connection.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Builder::new_local(&path).build().await.unwrap();
                let conn = db.connect().unwrap();
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS keys (
                        identity TEXT PRIMARY KEY,
                        pubkey   TEXT NOT NULL
                    )",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO keys (identity, pubkey) VALUES ('alice', 'aa11')",
                    (),
                )
                .await
                .unwrap();
            });
        }

        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        // First open runs the migration.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(s.get_keys("alice").unwrap(), vec!["aa11".to_string()]);
            assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));
        }
        // Re-open: idempotent copy — still exactly one key.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(s.get_keys("alice").unwrap(), vec!["aa11".to_string()]);
        }
    }

    /// End-to-end pull on libsql: A enqueues for B; B pulls read-only and commits
    /// into its own inbox; a re-pull is idempotent; A's main DB file is
    /// byte-unchanged (the owner-only-writes structural proof on libsql).
    #[test]
    fn pull_from_store_commits_once_and_leaves_source_unchanged_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-pull-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");

        {
            let cfg = Config {
                db: Some(a_path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let a = LibsqlStore::open(&cfg).unwrap();
            a.enqueue_intent(
                "bob",
                "",
                "alice",
                Some("hi"),
                "hello bob",
                "",
                None,
                None,
                None,
                0,
            )
            .unwrap();
        }
        // Snapshot A's main DB file BEFORE B pulls (WAL legitimately appears on a
        // read-only open; the invariant is asserted on the main data file + empty WAL).
        let before = std::fs::read(&a_path).unwrap();

        let b = {
            let cfg = Config {
                db: Some(b_path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            LibsqlStore::open(&cfg).unwrap()
        };
        let allow = vec![StoreSource::Local(a_path.clone())];
        let pulled = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(pulled.committed, 1);

        let (rows, _) = b.inbox("bob", false, false, 50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "alice");
        assert_eq!(rows[0].body, "hello bob");

        let again = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(again.committed, 0, "re-drain must not double-deliver");
        let (rows2, _) = b.inbox("bob", false, false, 50).unwrap();
        assert_eq!(rows2.len(), 1);

        // OWNER-ONLY-WRITES: A's main DB is byte-identical; its WAL is empty/absent.
        let after = std::fs::read(&a_path).unwrap();
        assert_eq!(
            before, after,
            "pulling must leave the source main DB byte-unchanged (libsql)"
        );
        let wal_len = std::fs::metadata(format!("{}-wal", a_path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "no write committed to the source (WAL empty)");
    }

    // -----------------------------------------------------------------------
    // #11 observed-revocation audit log — libSQL backend mirror of the sqlite
    // store-unit tests. Plain data in every build (not sign-gated for the
    // round-trip/clamp/limit/migration/owner-only cases).
    // -----------------------------------------------------------------------

    /// libSQL `record_revocation` + `list_revocations` round-trip most-recent-first;
    /// `count_revocations` matches; oversized `fp`/`source`/`identity` are clamped at
    /// the write seam; `limit` is bounded (negative ⇒ 0, over-cap ⇒ cap).
    #[test]
    fn revocations_roundtrip_clamp_and_bounds_libsql() {
        use crate::store::{RevocationEvent, RevocationKind, MAX_REVOCATION_FIELD_LEN};
        let s = mem();
        assert_eq!(s.count_revocations().unwrap(), 0);
        assert!(s.list_revocations(50).unwrap().is_empty());

        s.record_revocation(&RevocationEvent {
            id: 0,
            ts: 100,
            fp: "SHA256:aa".into(),
            identity: "alice".into(),
            source: "local:/a".into(),
            kind: RevocationKind::Enforced,
        })
        .unwrap();
        s.record_revocation(&RevocationEvent {
            id: 0,
            ts: 200,
            fp: "SHA256:bb".into(),
            identity: String::new(),
            source: String::new(),
            kind: RevocationKind::Declared,
        })
        .unwrap();
        assert_eq!(s.count_revocations().unwrap(), 2);
        let rows = s.list_revocations(50).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fp, "SHA256:bb", "most-recent-first (id DESC)");
        assert_eq!(rows[0].kind, RevocationKind::Declared);
        assert_eq!(rows[1].fp, "SHA256:aa");
        assert_eq!(rows[1].identity, "alice");
        assert_eq!(rows[1].source, "local:/a");
        assert_eq!(rows[1].kind, RevocationKind::Enforced);

        // Clamp at the seam.
        let huge = "x".repeat(MAX_REVOCATION_FIELD_LEN + 300);
        s.record_revocation(&RevocationEvent {
            id: 0,
            ts: 300,
            fp: huge.clone(),
            identity: huge.clone(),
            source: huge.clone(),
            kind: RevocationKind::Enforced,
        })
        .unwrap();
        let r = s.list_revocations(1).unwrap();
        assert!(r[0].fp.len() <= MAX_REVOCATION_FIELD_LEN, "fp clamped");
        assert!(
            r[0].source.len() <= MAX_REVOCATION_FIELD_LEN,
            "source clamped"
        );
        assert!(
            r[0].identity.len() <= MAX_REVOCATION_FIELD_LEN,
            "identity clamped"
        );

        // Bounds: small truncates, negative ⇒ 0, over-cap ⇒ available (3).
        assert_eq!(s.list_revocations(1).unwrap().len(), 1);
        assert_eq!(s.list_revocations(0).unwrap().len(), 0);
        assert_eq!(
            s.list_revocations(-5).unwrap().len(),
            0,
            "negative ⇒ 0 (bounded)"
        );
        assert_eq!(
            s.list_revocations(1_000_000).unwrap().len(),
            3,
            "over-cap clamps"
        );
    }

    /// OWNER-ONLY-WRITES on libsql: `record_revocation` traps on a `read_only`
    /// (foreign/remote) handle — never a write, never a panic — and the foreign DB
    /// file is byte-unchanged. Reads (`list`/`count`) still work on the same handle.
    #[test]
    fn record_revocation_traps_on_readonly_handle_libsql() {
        use crate::store::{RevocationEvent, RevocationKind};
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-revguard-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foreign.db");
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            // Seed one event so list/count have something to read on the RO handle.
            rw.record_revocation(&RevocationEvent {
                id: 0,
                ts: 1,
                fp: "SHA256:seed".into(),
                identity: String::new(),
                source: String::new(),
                kind: RevocationKind::Declared,
            })
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(ro.read_only, "open_readonly sets the guard flag");
        let ev = RevocationEvent {
            id: 0,
            ts: 2,
            fp: "SHA256:intruder".into(),
            identity: String::new(),
            source: String::new(),
            kind: RevocationKind::Enforced,
        };
        let e = ro
            .record_revocation(&ev)
            .expect_err("record_revocation must trap on a read-only handle");
        assert!(
            e.to_string()
                .contains("BUG: write attempted on a read-only foreign store"),
            "wrong trap error: {e}"
        );
        // Reads still work on the RO handle.
        assert_eq!(
            ro.count_revocations().unwrap(),
            1,
            "the seeded event is readable"
        );
        drop(ro);
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "trapped write left the foreign DB byte-unchanged"
        );
    }

    /// Migration on libsql: a legacy DB that predates `revocations` gains the table
    /// idempotently on open (mirror of the sqlite legacy-migration test); a
    /// pre-existing peers row survives (no data loss) and re-opening is a no-op.
    #[test]
    fn legacy_db_gains_revocations_table_libsql() {
        use crate::store::{RevocationEvent, RevocationKind};
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-revlegacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // Build a pre-#11 DB: a peers table + one row, NO revocations table.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Builder::new_local(&path).build().await.unwrap();
                let conn = db.connect().unwrap();
                conn.execute(
                    "CREATE TABLE peers (
                        name TEXT PRIMARY KEY, mux TEXT NOT NULL, target TEXT NOT NULL,
                        socket TEXT NOT NULL DEFAULT '', cwd TEXT, last_seen INTEGER NOT NULL
                     )",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO peers (name, mux, target, socket, cwd, last_seen)
                     VALUES ('old', 'tmux', '%1', '', '/legacy', ?1)",
                    params(vec![now().into()]),
                )
                .await
                .unwrap();
            });
        }

        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        // First open runs the inline migration: the table exists and is usable, and
        // the pre-existing peer survives.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(s.count_revocations().unwrap(), 0, "table created, empty");
            s.record_revocation(&RevocationEvent {
                id: 0,
                ts: 9,
                fp: "SHA256:zz".into(),
                identity: "id".into(),
                source: "src".into(),
                kind: RevocationKind::Declared,
            })
            .unwrap();
            assert_eq!(s.count_revocations().unwrap(), 1);
            assert!(
                s.get_peer("old").unwrap().is_some(),
                "legacy peer survived the migration (no data loss)"
            );
        }
        // Re-open: idempotent — the recorded event persists, no duplicate-table error.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(s.count_revocations().unwrap(), 1, "re-open is a no-op");
        }
    }

    /// Migration on libsql (P3): a genuine LEGACY DB that predates the `jobs` table
    /// gains it on open (mirror of the sqlite `legacy_db_gains_jobs_table`); a
    /// pre-existing peers row survives (no data loss) and re-opening is a clean
    /// no-op. Proves the inline migrate upgrade path, not just `IF NOT EXISTS`
    /// re-entry on a DB that already has the table.
    #[test]
    fn legacy_db_gains_jobs_table_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-jobslegacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // Build a pre-P3 DB: a peers table + one row, and NO jobs table.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Builder::new_local(&path).build().await.unwrap();
                let conn = db.connect().unwrap();
                conn.execute(
                    "CREATE TABLE peers (
                        name TEXT PRIMARY KEY, mux TEXT NOT NULL, target TEXT NOT NULL,
                        socket TEXT NOT NULL DEFAULT '', cwd TEXT, last_seen INTEGER NOT NULL
                     )",
                    (),
                )
                .await
                .unwrap();
                conn.execute(
                    "INSERT INTO peers (name, mux, target, socket, cwd, last_seen)
                     VALUES ('old', 'tmux', '%1', '', '/legacy', ?1)",
                    params(vec![now().into()]),
                )
                .await
                .unwrap();
            });
        }

        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        // First open runs the inline migration: the jobs table exists and is usable,
        // and the pre-existing peer survives.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(
                s.list_jobs(JobFilter::default(), 100).unwrap().len(),
                0,
                "jobs table created, empty"
            );
            let j = s.create_job("alice", jspec("after-migrate")).unwrap();
            assert!(s.get_job(&j.id).unwrap().is_some());
            assert!(
                s.get_peer("old").unwrap().is_some(),
                "legacy peer survived the migration (no data loss)"
            );
        }
        // Re-open: idempotent — the created job persists, no duplicate-table error.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            assert_eq!(
                s.list_jobs(JobFilter::default(), 100).unwrap().len(),
                1,
                "re-open is a no-op; the created job persists"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P3 job board store tests (libsql) — mirror the sqlite parity tests ────

    fn jspec(title: &str) -> JobSpec {
        JobSpec {
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn job_create_claim_update_result_roundtrip_libsql() {
        let s = mem();
        let j = s.create_job("alice", jspec("task")).unwrap();
        assert!(crate::model::job_id_valid(&j.id));
        assert_eq!(j.state, JobState::Queued);
        assert_eq!(j.owner.as_deref(), Some("alice")); // owner defaults to creator

        let att = s
            .claim_job(&j.id, "w")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        assert!(crate::model::attempt_id_valid(&att));

        let done = s
            .update_job(
                &j.id,
                Some(&att),
                JobPatch {
                    state: Some(JobState::Completed),
                    result_summary: Some("ok".into()),
                    progress_note: Some("fin".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(done.state, JobState::Completed);
        assert!(done.completed_ts.is_some());
        assert!(done.progress_events_json.contains("fin"));

        let r = s.job_result(&j.id).unwrap().unwrap();
        assert!(r.ready);
    }

    #[test]
    fn job_dispatch_claim_is_queued_only_and_preserves_manual_reclaim_libsql() {
        let s = mem();
        let j = s.create_job("alice", jspec("task")).unwrap();
        let first = s.claim_queued_job(&j.id, "worker").unwrap().unwrap();
        let first_attempt = first.attempt_id.clone().unwrap();
        assert_eq!(first.state, JobState::Running);

        assert!(
            s.claim_queued_job(&j.id, "worker").unwrap().is_none(),
            "a stale/concurrent dispatch cannot reclaim a running row"
        );
        let manual = s.claim_job(&j.id, "recovery").unwrap().unwrap();
        assert_ne!(manual.attempt_id.as_deref(), Some(first_attempt.as_str()));
        assert_eq!(manual.assignee.as_deref(), Some("recovery"));

        let assigned = s
            .create_job(
                "alice",
                JobSpec {
                    title: "assigned elsewhere".into(),
                    assignee: Some("other".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(s
            .claim_queued_job(&assigned.id, "worker")
            .unwrap()
            .is_none());
        let unchanged = s.get_job(&assigned.id).unwrap().unwrap();
        assert_eq!(unchanged.state, JobState::Queued);
        assert!(unchanged.attempt_id.is_none());
    }

    #[test]
    fn concurrent_job_dispatch_claim_has_exactly_one_winner_libsql() {
        use std::sync::{Arc, Barrier};

        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-dispatch-claim-race-{}-{}",
            std::process::id(),
            crate::model::new_attempt_id(now())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let open = |path: &std::path::Path| {
            LibsqlStore::open(&Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".into()),
                ..Config::default()
            })
            .unwrap()
        };
        let seed = open(&path);
        let id = seed.create_job("alice", jspec("race")).unwrap().id;
        drop(seed);

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for worker in ["worker-a", "worker-b"] {
            let path = path.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let store = LibsqlStore::open(&Config {
                    db: Some(path.to_string_lossy().into_owned()),
                    backend: Some("libsql".into()),
                    ..Config::default()
                })
                .unwrap();
                barrier.wait();
                store.claim_queued_job(&id, worker)
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|job| job.is_some()).count(),
            1,
            "the state predicate and transition must be one atomic write"
        );

        let store = open(&path);
        assert_eq!(
            store.get_job(&id).unwrap().unwrap().state,
            JobState::Running
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn job_text_nul_is_rejected_libsql() {
        let s = mem();
        assert!(s.create_job("alice", jspec("bad\0title")).is_err());
    }

    #[test]
    fn job_stale_attempt_is_fenced_libsql() {
        let s = mem();
        let j = s.create_job("alice", jspec("task")).unwrap();
        let old = s
            .claim_job(&j.id, "w1")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        let new = s
            .claim_job(&j.id, "w2")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        assert_ne!(old, new);
        let err = s
            .update_job(
                &j.id,
                Some(&old),
                JobPatch {
                    state: Some(JobState::Completed),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("stale_attempt"));
    }

    #[test]
    fn job_illegal_transition_and_cancel_libsql() {
        let s = mem();
        // illegal transition: completed -> running.
        let j = s.create_job("alice", jspec("task")).unwrap();
        let att = s
            .claim_job(&j.id, "w")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        s.update_job(
            &j.id,
            Some(&att),
            JobPatch {
                state: Some(JobState::Completed),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s
            .update_job(
                &j.id,
                Some(&att),
                JobPatch {
                    state: Some(JobState::Running),
                    ..Default::default()
                }
            )
            .is_err());

        // cancel: queued -> terminal cancelled; running -> flag only.
        let q = s.create_job("alice", jspec("q")).unwrap();
        let c = s.cancel_job(&q.id, "alice", None).unwrap().unwrap();
        assert_eq!(c.state, JobState::Cancelled);
        let r = s.create_job("alice", jspec("r")).unwrap();
        s.claim_job(&r.id, "w").unwrap();
        let rc = s.cancel_job(&r.id, "alice", None).unwrap().unwrap();
        assert_eq!(rc.state, JobState::Running);
        assert!(rc.cancel_requested);
    }

    #[test]
    fn job_list_filters_libsql() {
        let s = mem();
        let a = s.create_job("alice", jspec("a")).unwrap();
        s.create_job("bob", jspec("b")).unwrap();
        s.claim_job(&a.id, "alice").unwrap();
        let running = s
            .list_jobs(
                JobFilter {
                    state: Some(JobState::Running),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        assert_eq!(running.len(), 1);
        let all = s.list_jobs(JobFilter::default(), i64::MAX).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn job_readonly_handle_traps_writes_libsql() {
        // A read-only foreign handle must REFUSE every job write (owner-only-writes).
        let s = mem();
        let j = s.create_job("alice", jspec("task")).unwrap();
        drop(s);
        // Re-open the same path read-only and assert writes trap.
        // (mem() makes a unique dir each call, so re-derive the path is not trivial;
        //  instead assert the guard via a fresh read-only store over a known file.)
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-jobro-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let w = LibsqlStore::open(&cfg).unwrap();
            w.create_job("alice", jspec("seed")).unwrap();
        }
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(ro.create_job("alice", jspec("nope")).is_err());
        assert!(ro.claim_job(&j.id, "w").is_err());
        assert!(ro.claim_queued_job(&j.id, "w").is_err());
        assert!(ro.cancel_job(&j.id, "alice", None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────────── P5: rich presence parity (libsql backend) ────────────

    /// libsql parity: a fresh peer takes the presence defaults; set_turn_state
    /// round-trips each enum value SELF-ONLY and rejects an unknown value with NO
    /// write; set_description round-trips + sanitizes + caps SELF-ONLY.
    #[test]
    fn presence_setters_roundtrip_self_only_libsql() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.register_peer("b", "tmux", "%2", "", Some("/y")).unwrap();
        // Fresh defaults.
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(
            (
                p.turn_state.as_str(),
                p.description.as_str(),
                p.description_ts
            ),
            ("", "", 0)
        );

        // turn_state round-trip, self-only, unknown-reject.
        s.set_turn_state("a", "working").unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().turn_state, "working");
        assert_eq!(s.get_peer("b").unwrap().unwrap().turn_state, "");
        assert!(s.set_turn_state("a", "garbage").is_err());
        assert_eq!(s.get_peer("a").unwrap().unwrap().turn_state, "working");

        // description round-trip, sanitize (control-strip), cap, self-only.
        s.set_description("a", "review\u{1b}[2J PR\u{0}").unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.description, "review[2J PR");
        assert!(p.description_ts > 0);
        assert_eq!(s.get_peer("b").unwrap().unwrap().description, "");
        let huge = "z".repeat(crate::model::MAX_DESC_LEN + 300);
        s.set_description("a", &huge).unwrap();
        assert!(
            s.get_peer("a")
                .unwrap()
                .unwrap()
                .description
                .chars()
                .count()
                <= crate::model::MAX_DESC_LEN
        );
        // Clearing stamps ts=0.
        s.set_description("a", "").unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!((p.description.as_str(), p.description_ts), ("", 0));
    }

    /// libsql parity: read-time TTL blanks a stale description on get_peer/list_peers
    /// WITHOUT a DB write (the raw stored column is untouched); a fresh one within the
    /// window is honored.
    #[test]
    fn description_expires_at_read_time_libsql() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.set_description("a", "stale task").unwrap();
        // Poke description_ts past the TTL window via a direct UPDATE (test-only).
        let stale = now() - crate::model::DESCRIPTION_TTL_SECS - 1;
        s.rt.block_on(async {
            s.conn
                .execute(
                    "UPDATE peers SET description_ts=?2 WHERE name=?1",
                    params(vec!["a".into(), stale.into()]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap();
        // Read paths see it absent (expired).
        assert_eq!(s.get_peer("a").unwrap().unwrap().description, "");
        assert_eq!(
            s.list_peers()
                .unwrap()
                .iter()
                .find(|p| p.name == "a")
                .unwrap()
                .description,
            ""
        );
        // The STORED row is untouched (no read-time write).
        let (raw_desc, raw_ts) =
            s.rt.block_on(async {
                let mut it = s
                    .conn
                    .query(
                        "SELECT description, description_ts FROM peers WHERE name='a'",
                        (),
                    )
                    .await?;
                let r = it.next().await?.unwrap();
                Ok::<(String, i64), anyhow::Error>((r.get::<String>(0)?, r.get::<i64>(1)?))
            })
            .unwrap();
        assert_eq!(
            raw_desc, "stale task",
            "stored row not mutated at read time"
        );
        assert_eq!(raw_ts, stale);
        // A fresh description within the window is honored.
        s.set_description("a", "fresh task").unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().description, "fresh task");
    }

    /// libsql parity: register_peer_full re-register PRESERVES a self-set
    /// turn_state + description (the role-omitted-from-upsert discipline).
    #[test]
    fn reregister_preserves_presence_libsql() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.set_turn_state("a", "working").unwrap();
        s.set_description("a", "deep work").unwrap();
        let ts_before = s.get_peer("a").unwrap().unwrap().description_ts;
        let cert = s.get_birth_cert("a").unwrap().unwrap();
        s.register_peer_full(
            "a",
            "tmux",
            "%9",
            "",
            Some("/x"),
            Some(1234),
            "host",
            "repo",
            "br",
            "wt",
            "default",
            Some(&cert),
            "",
        )
        .unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.target, "%9");
        assert_eq!(
            p.turn_state, "working",
            "turn_state preserved across re-register"
        );
        assert_eq!(
            p.description, "deep work",
            "description preserved across re-register"
        );
        assert_eq!(p.description_ts, ts_before);
    }

    /// libsql parity: a legacy DB predating the three presence columns upgrades in
    /// place on open — columns added with the correct defaults, the old row survives
    /// reading Unknown/empty/0, the setters work, and a re-open is an idempotent no-op.
    #[test]
    fn legacy_db_gains_presence_columns_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-presence-legacy-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        // A pre-P5 store: open normally (creates the full P5 schema), then DROP the
        // three presence columns is not trivial in SQLite — instead recreate a peers
        // table without them via a fresh raw connection.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            s.rt.block_on(async {
                s.conn.execute("DROP TABLE peers", ()).await?;
                s.conn
                    .execute(
                        "CREATE TABLE peers (
                            name TEXT PRIMARY KEY, mux TEXT NOT NULL, target TEXT NOT NULL,
                            socket TEXT NOT NULL DEFAULT '', cwd TEXT NOT NULL DEFAULT '',
                            last_seen INTEGER NOT NULL, pid INTEGER, host TEXT NOT NULL DEFAULT '',
                            repo TEXT NOT NULL DEFAULT '', branch TEXT NOT NULL DEFAULT '',
                            worktree_id TEXT NOT NULL DEFAULT '', circle TEXT NOT NULL DEFAULT 'default',
                            role TEXT NOT NULL DEFAULT 'peer'
                         )",
                        (),
                    )
                    .await?;
                s.conn
                    .execute(
                        "INSERT INTO peers (name, mux, target, last_seen) VALUES ('old','tmux','%1',1)",
                        (),
                    )
                    .await?;
                Ok::<(), anyhow::Error>(())
            })
            .unwrap();
        }
        // Re-open migrates the legacy table in place.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(
                (
                    p.turn_state.as_str(),
                    p.description.as_str(),
                    p.description_ts
                ),
                ("", "", 0)
            );
            s.set_turn_state("old", "idle").unwrap();
            s.set_description("old", "post-migrate").unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(p.turn_state, "idle");
            assert_eq!(p.description, "post-migrate");
        }
        // Re-open is an idempotent no-op; the set presence persists.
        {
            let s = LibsqlStore::open(&cfg).unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(p.turn_state, "idle");
            assert_eq!(p.description, "post-migrate");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// libsql parity: the presence setters TRAP through a read-only handle (the
    /// guard_writable write-trap, FIRST in each setter) — never a silent foreign
    /// write, never a panic.
    #[test]
    fn presence_setters_trap_on_readonly_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-presence-ro-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.db");
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let rw = LibsqlStore::open(&cfg).unwrap();
            rw.register_peer("seed", "tmux", "%1", "", Some("/w"))
                .unwrap();
        }
        let ro = LibsqlStore::open_readonly(&path).unwrap();
        assert!(
            ro.set_turn_state("seed", "working").is_err(),
            "set_turn_state through a read-only handle must trap"
        );
        assert!(
            ro.set_description("seed", "intruder").is_err(),
            "set_description through a read-only handle must trap"
        );
        // The failed writes were no-ops.
        let p = ro.get_peer("seed").unwrap().unwrap();
        assert_eq!(p.turn_state, "");
        assert_eq!(p.description, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Presence seam (v0.2) — libsql parity
    // -----------------------------------------------------------------------

    #[test]
    fn presence_heartbeat_and_query_libsql() {
        let s = mem();
        let host = crate::config::this_host();
        assert!(s.presence("alice", &host).unwrap().is_none());
        s.heartbeat("alice", &host, Some(1234)).unwrap();
        let ts = s
            .presence("alice", &host)
            .unwrap()
            .expect("fresh heartbeat");
        assert!(ts > 0);
        assert!(s.presence("alice", "other-box").unwrap().is_none());
        let n = s
            .evict_stale_presence(crate::store::PRESENCE_TTL_SECS)
            .unwrap();
        assert_eq!(n, 0);
        assert!(s.presence("alice", &host).unwrap().is_some());
    }

    #[test]
    fn presence_evict_stale_libsql() {
        let s = mem();
        let host = crate::config::this_host();
        let old_ts = crate::model::now() - crate::store::PRESENCE_TTL_SECS - 1;
        s.rt.block_on(async {
            s.conn
                .execute(
                    "INSERT INTO presence (name, host, pid, heartbeat_ts) VALUES (?1, ?2, ?3, ?4)",
                    params(vec![
                        "bob".into(),
                        host.clone().into(),
                        0i64.into(),
                        old_ts.into(),
                    ]),
                )
                .await
                .unwrap();
        });
        assert!(s.presence("bob", &host).unwrap().is_none());
        let n = s
            .evict_stale_presence(crate::store::PRESENCE_TTL_SECS)
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn peer_liveness_three_tier_libsql() {
        let s = mem();
        let host = crate::config::this_host();
        s.heartbeat("carol", &host, Some(1234)).unwrap();
        let p = Peer {
            name: "carol".into(),
            mux: "tmux".into(),
            target: "%1".into(),
            socket: String::new(),
            cwd: None,
            last_seen: 0,
            pid: Some(1234),
            host: host.clone(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: crate::model::DEFAULT_CIRCLE.to_string(),
            role: crate::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
            client_session: String::new(),
        };
        assert_eq!(
            s.peer_liveness(&p).unwrap(),
            crate::model::Liveness::Live,
            "heartbeat wins over stale last_seen"
        );

        let p2 = Peer {
            name: "dave".into(),
            last_seen: crate::model::now(),
            pid: None,
            ..p.clone()
        };
        assert_eq!(
            s.peer_liveness(&p2).unwrap(),
            crate::model::Liveness::Likely
        );

        let p3 = Peer {
            name: "eve".into(),
            last_seen: 0,
            pid: None,
            ..p.clone()
        };
        assert_eq!(
            s.peer_liveness(&p3).unwrap(),
            crate::model::Liveness::Offline
        );
    }

    // ── WL-016 schedule store tests (libsql mirror) ──────────────────────────

    #[test]
    fn schedule_one_shot_roundtrip_libsql() {
        let s = mem();
        let id = s
            .schedule_message(
                "alice",
                "bob",
                Some("hi"),
                "hello",
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000,
            )
            .unwrap();
        let list = s.list_schedules("alice", 50).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].kind, ScheduleKind::OneShot);
        assert_eq!(list[0].recipient, "bob");
    }

    #[test]
    fn schedule_cancel_libsql() {
        let s = mem();
        let id = s
            .schedule_message(
                "a",
                "b",
                None,
                "x",
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000,
            )
            .unwrap();
        assert!(s.cancel_schedule(id).unwrap());
        assert!(!s.cancel_schedule(id).unwrap());
        let list = s.list_schedules("a", 50).unwrap();
        assert!(list[0].cancelled);
    }

    #[test]
    fn schedule_due_query_libsql() {
        let s = mem();
        let past = crate::model::now() - 3600;
        let id = s
            .schedule_message("a", "b", None, "x", ScheduleKind::OneShot, "@daily", past)
            .unwrap();
        let due = s.get_due_schedules(crate::model::now()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
    }

    #[test]
    fn schedule_mark_executed_one_shot_libsql() {
        let s = mem();
        let id = s
            .schedule_message(
                "a",
                "b",
                None,
                "x",
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000,
            )
            .unwrap();
        s.mark_schedule_executed(id).unwrap();
        let list = s.list_schedules("a", 50).unwrap();
        assert!(list[0].executed_ts.is_some());
    }

    #[test]
    fn schedule_mark_executed_recurring_libsql() {
        let s = mem();
        let past = crate::model::now() - 3600;
        let id = s
            .schedule_message(
                "a",
                "b",
                None,
                "x",
                ScheduleKind::Recurring,
                "@hourly",
                past,
            )
            .unwrap();
        s.mark_schedule_executed(id).unwrap();
        let list = s.list_schedules("a", 50).unwrap();
        assert!(list[0].executed_ts.is_none());
        assert!(list[0].next_run > past);
    }

    // ---- WL-020: review queue ----

    #[test]
    fn review_add_list_mark_remove_roundtrip_libsql() {
        let s = mem();
        let id = s
            .add_review_item(
                "https://github.com/owner/repo/pull/1",
                "fix bug",
                "alice",
                "owner/repo",
                crate::model::ReviewItemState::Open,
                None,
            )
            .unwrap();
        assert!(id.starts_with("review_"));

        let all = s
            .review_queue(crate::model::ReviewQueueFilter::All, 10)
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pr_url, "https://github.com/owner/repo/pull/1");

        let pending = s
            .review_queue(crate::model::ReviewQueueFilter::Pending, 10)
            .unwrap();
        assert_eq!(pending.len(), 1);

        assert!(s.mark_reviewed(&id, "bob").unwrap());
        let reviewed = s
            .review_queue(crate::model::ReviewQueueFilter::Reviewed, 10)
            .unwrap();
        assert_eq!(reviewed.len(), 1);
        assert_eq!(reviewed[0].reviewed_by, Some("bob".to_string()));

        assert!(s.remove_review_item(&id).unwrap());
        let all = s
            .review_queue(crate::model::ReviewQueueFilter::All, 10)
            .unwrap();
        assert_eq!(all.len(), 0);
    }

    #[test]
    fn review_rejects_bad_url_libsql() {
        let s = mem();
        assert!(s
            .add_review_item(
                "not-a-url",
                "t",
                "a",
                "r",
                crate::model::ReviewItemState::Open,
                None
            )
            .is_err());
    }

    // ---- WL-021: permission status ----

    #[test]
    fn permission_verdict_approved_after_answer_libsql() {
        let s = mem();
        let (cid, _qid) = s
            .ask(
                "alice",
                "bob",
                None,
                "allow rm?",
                crate::model::AskKind::ToolPermission,
                Some("Bash\nrm -rf /"),
                None,
            )
            .unwrap();
        s.answer("bob", &cid, "approve").unwrap();
        let (status, body) = s.permission_verdict(&cid, 300).unwrap();
        assert_eq!(status, crate::model::PermissionStatus::Approved);
        assert_eq!(body.unwrap(), "approve");
    }

    #[test]
    fn permission_list_filters_by_asker_libsql() {
        let s = mem();
        s.ask(
            "alice",
            "bob",
            None,
            "q1",
            crate::model::AskKind::ToolPermission,
            None,
            None,
        )
        .unwrap();
        s.ask(
            "alice",
            "bob",
            None,
            "q2",
            crate::model::AskKind::FreeText,
            None,
            None,
        )
        .unwrap();
        let perms = s.list_permissions("alice", 10).unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].kind, crate::model::AskKind::ToolPermission);
    }

    #[test]
    fn summary_roundtrip_libsql() {
        let s = mem();
        let root = s.send("a", "b", None, "root", None, None).unwrap();
        assert!(s.get_summary(root).unwrap().is_none());
        s.store_summary(root, "summary text", "gpt-4").unwrap();
        let sum = s.get_summary(root).unwrap().unwrap();
        assert_eq!(sum.root_id, root);
        assert_eq!(sum.text, "summary text");
        assert_eq!(sum.model, "gpt-4");
        // Upsert refreshes
        s.store_summary(root, "new text", "gpt-3").unwrap();
        let sum2 = s.get_summary(root).unwrap().unwrap();
        assert_eq!(sum2.text, "new text");
        assert_eq!(sum2.model, "gpt-3");
        assert!(s.delete_summary(root).unwrap());
        assert!(!s.delete_summary(root).unwrap());
        assert!(s.get_summary(root).unwrap().is_none());

        s.store_summary(root + 10_000, "orphan", "legacy").unwrap();
        assert!(
            s.get_summary(root + 10_000).unwrap().is_none(),
            "a cache row without a live root message must never surface"
        );
    }

    #[test]
    fn legacy_summary_cache_migrates_fail_closed_and_current_generation_roundtrips_libsql() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-summary-legacy-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };

        let root = {
            let s = LibsqlStore::open(&cfg).unwrap();
            let root = s.send("a", "b", None, "root", None, None).unwrap();
            s.rt.block_on(async {
                for ddl in [
                    "DROP TRIGGER summaries_generation_message_insert_v1",
                    "DROP TRIGGER summaries_generation_message_update_v1",
                    "DROP TRIGGER summaries_generation_message_delete_v1",
                    "DROP TABLE summaries",
                    "DROP TABLE summary_state",
                    "CREATE TABLE summaries (
                        root_id      INTEGER PRIMARY KEY,
                        text         TEXT NOT NULL,
                        model        TEXT NOT NULL DEFAULT '',
                        created_ts   INTEGER NOT NULL,
                        refreshed_ts INTEGER NOT NULL
                    )",
                ] {
                    s.conn.execute(ddl, ()).await?;
                }
                let ts = now();
                s.conn
                    .execute(
                        "INSERT INTO summaries
                             (root_id, text, model, created_ts, refreshed_ts)
                         VALUES (?1, 'legacy cache', 'legacy-model', ?2, ?2)",
                        params(vec![root.into(), ts.into()]),
                    )
                    .await?;
                Ok::<(), anyhow::Error>(())
            })
            .unwrap();
            root
        };

        let s = LibsqlStore::open(&cfg).unwrap();
        let legacy_generation =
            s.rt.block_on(async {
                let mut rows = s
                    .conn
                    .query(
                        "SELECT generation FROM summaries WHERE root_id = ?1",
                        params(vec![root.into()]),
                    )
                    .await?;
                let row = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("legacy cache row disappeared"))?;
                Ok::<i64, anyhow::Error>(row.get(0)?)
            })
            .unwrap();
        assert_eq!(legacy_generation, -1, "legacy cache must migrate stale");
        assert!(
            s.get_summary(root).unwrap().is_none(),
            "a pre-generation cache row must fail closed"
        );
        let generation = s.summary_generation().unwrap();
        assert!(s
            .store_summary_if_generation(root, "current cache", "current-model", generation)
            .unwrap());
        assert_eq!(s.get_summary(root).unwrap().unwrap().text, "current cache");

        let before_reply = s.summary_generation().unwrap();
        s.reply("b", root, "new reply").unwrap();
        assert!(s.summary_generation().unwrap() > before_reply);
        assert!(
            s.get_summary(root).unwrap().is_none(),
            "migrated invalidation triggers must clear cache on message mutation"
        );
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_all_removes_summary_cache_libsql() {
        let s = mem();
        let root = s.send("a", "b", None, "root", None, None).unwrap();
        s.store_summary(root, "cached", "model").unwrap();
        assert_eq!(s.clear_all().unwrap(), 1);
        assert!(
            !s.delete_summary(root).unwrap(),
            "clear_all must delete the underlying cache row"
        );
    }

    #[test]
    fn adding_reply_invalidates_cached_thread_summary_libsql() {
        let s = mem();
        let root = s.send("a", "b", None, "root", None, None).unwrap();
        s.store_summary(root, "cached", "model").unwrap();
        assert!(s.get_summary(root).unwrap().is_some());

        s.reply("b", root, "new reply").unwrap();
        assert!(
            s.get_summary(root).unwrap().is_none(),
            "a new descendant makes the cached thread summary stale"
        );
    }

    #[test]
    fn summary_generation_rejects_mutated_or_deleted_snapshots_libsql() {
        let s = mem();
        let root = s.send("a", "b", None, "root", None, None).unwrap();
        let (initial_rows, initial_generation) =
            crate::store::summary_thread_snapshot(&s, root, 200).unwrap();
        assert_eq!(initial_rows.len(), 1);

        let reply = s.reply("b", root, "new reply").unwrap();
        assert!(
            !s.store_summary_if_generation(root, "stale", "model", initial_generation)
                .unwrap(),
            "a reply inserted during provider work must reject the stale write"
        );
        assert!(s.get_summary(root).unwrap().is_none());

        s.set_message_expiry(reply, now() - 1).unwrap();
        let before_delete = s.summary_generation().unwrap();
        assert_eq!(s.sweep_expired_messages().unwrap(), 1);
        assert!(
            !s.store_summary_if_generation(root, "stale", "model", before_delete)
                .unwrap(),
            "a reply deleted during provider work must reject the stale write"
        );

        let (current_rows, current_generation) =
            crate::store::summary_thread_snapshot(&s, root, 200).unwrap();
        assert_eq!(current_rows.len(), 1, "expired reply is never summarized");
        assert!(s
            .store_summary_if_generation(root, "current", "model", current_generation)
            .unwrap());
        assert_eq!(s.get_summary(root).unwrap().unwrap().text, "current");
    }

    #[test]
    fn gc_message_delete_invalidates_the_whole_summary_cache_libsql() {
        let s = mem();
        let old_root = s.send("a", "b", None, "old", None, None).unwrap();
        let live_root = s.send("c", "d", None, "live", None, None).unwrap();
        s.rt.block_on(async {
            s.conn
                .execute(
                    "UPDATE messages SET ts = ts - 100000 WHERE id = ?1",
                    params(vec![old_root.into()]),
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        })
        .unwrap();
        s.store_summary(old_root, "old cached", "model").unwrap();
        s.store_summary(live_root, "live cached", "model").unwrap();

        assert_eq!(s.gc(3600).unwrap(), 1);
        assert!(s.get_summary(live_root).unwrap().is_none());
        assert!(
            !s.delete_summary(old_root).unwrap(),
            "gc must delete cache rows, not merely hide orphan roots"
        );
    }

    #[test]
    fn expiry_sweep_of_root_or_reply_invalidates_the_whole_summary_cache_libsql() {
        let s = mem();
        let live_root = s.send("a", "b", None, "live root", None, None).unwrap();
        let expired_reply = s.reply("b", live_root, "expired reply").unwrap();
        let expired_root = s.send("c", "d", None, "expired root", None, None).unwrap();
        s.set_message_expiry(expired_reply, now() - 5).unwrap();
        s.set_message_expiry(expired_root, now() - 5).unwrap();
        s.store_summary(live_root, "live cached", "model").unwrap();
        s.store_summary(expired_root, "expired cached", "model")
            .unwrap();

        assert_eq!(s.sweep_expired_messages().unwrap(), 2);
        assert!(
            s.history("a", None, 100)
                .unwrap()
                .iter()
                .any(|m| m.id == live_root),
            "the summarized root stays live when only its reply expires"
        );
        assert!(s.get_summary(live_root).unwrap().is_none());
        assert!(
            !s.delete_summary(expired_root).unwrap(),
            "sweep must delete the underlying orphan cache row"
        );
    }

    // ---- WL-037: supersede on the libsql backend (positional-projection trap)

    /// Count of `me`'s unread messages via the inbox unread branch (libsql has no
    /// inherent `unread_count`; the unread inbox query is the equivalent surface).
    fn unread_ids(s: &LibsqlStore, me: &str) -> Vec<i64> {
        s.inbox(me, false, false, 50)
            .unwrap()
            .0
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn superseded_by_of(s: &LibsqlStore, me: &str, id: i64) -> Option<i64> {
        s.history(me, None, 100)
            .unwrap()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("message #{id} not in {me} history"))
            .superseded_by
    }

    #[test]
    fn supersede_stamps_and_hides_from_unread_libsql() {
        let s = mem();
        let a = s.send("a", "b", Some("v1"), "first", None, None).unwrap();
        let b = s.send("a", "b", Some("v2"), "second", None, None).unwrap();
        assert_eq!(unread_ids(&s, "b").len(), 2);
        s.supersede("a", a, b).unwrap();
        // Positional projection (index 10) must align: predecessor stamped, hidden.
        assert_eq!(superseded_by_of(&s, "b", a), Some(b));
        assert_eq!(superseded_by_of(&s, "b", b), None);
        let unread = unread_ids(&s, "b");
        assert_eq!(unread, vec![b], "only the successor remains unread");
        // peek_oldest_unread (nudge/wake path) skips the superseded predecessor.
        assert_eq!(s.peek_oldest_unread("b").unwrap().unwrap().id, b);
    }

    #[test]
    fn supersede_chain_and_history_flag_libsql() {
        let s = mem();
        let a = s.send("a", "b", None, "A", None, None).unwrap();
        let b = s.send("a", "b", None, "B", None, None).unwrap();
        let c = s.send("a", "b", None, "C", None, None).unwrap();
        s.supersede("a", a, b).unwrap();
        s.supersede("a", b, c).unwrap();
        assert_eq!(unread_ids(&s, "b"), vec![c]);
        // history keeps the superseded rows AND populates the flag (positional read).
        let hist = s.history("b", None, 100).unwrap();
        assert_eq!(
            hist.iter().find(|m| m.id == a).unwrap().superseded_by,
            Some(b)
        );
        assert_eq!(
            hist.iter().find(|m| m.id == b).unwrap().superseded_by,
            Some(c)
        );
    }

    #[test]
    fn supersede_rejects_foreign_and_missing_libsql() {
        let s = mem();
        let a = s.send("a", "b", None, "A", None, None).unwrap();
        let newer = s.send("a", "b", None, "B", None, None).unwrap();
        let b = s.send("c", "b", None, "C", None, None).unwrap();
        assert!(s.supersede("a", a, a).is_err(), "self-link rejected");
        assert!(
            s.supersede("a", newer, a).is_err(),
            "backward link rejected"
        );
        assert!(s.supersede("c", a, b).is_err(), "foreign sender rejected");
        assert_eq!(superseded_by_of(&s, "b", a), None);
        assert!(
            s.supersede("a", 999_999, a).is_err(),
            "missing old rejected"
        );
        assert!(
            s.supersede("a", a, 999_999).is_err(),
            "missing new rejected"
        );
    }

    #[test]
    fn supersede_broadcast_drops_from_all_readers_libsql() {
        let s = mem();
        let bcast = "all";
        let a = s.send("a", bcast, None, "v1", None, None).unwrap();
        let b = s.send("a", bcast, None, "v2", None, None).unwrap();
        assert_eq!(unread_ids(&s, "r1").len(), 2);
        assert_eq!(unread_ids(&s, "r2").len(), 2);
        s.supersede("a", a, b).unwrap();
        assert_eq!(unread_ids(&s, "r1"), vec![b]);
        assert_eq!(unread_ids(&s, "r2"), vec![b]);
    }

    // ---- WL-039: idle-notification dedup (libsql, positional-projection trap)

    #[test]
    fn idle_dedup_replaces_prior_unread_idle_libsql() {
        let s = mem();
        let p1 = s
            .send("a", "b", None, "still waiting?", None, None)
            .unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", p1).unwrap(), 0);
        let p2 = s
            .send("a", "b", None, "still waiting??", None, None)
            .unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", p2).unwrap(), 1);
        // Only the latest is unread; the predecessor is stamped + hidden (the
        // positional `kind` projection at index 12 must align).
        assert_eq!(unread_ids(&s, "b"), vec![p2]);
        assert_eq!(superseded_by_of(&s, "b", p1), Some(p2));
        assert_eq!(s.peek_oldest_unread("b").unwrap().unwrap().id, p2);
    }

    #[test]
    fn idle_dedup_never_touches_real_messages_libsql() {
        let s = mem();
        let p1 = s.send("a", "b", None, "ping 1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", p1).unwrap();
        let real = s.send("a", "b", Some("work"), "real", None, None).unwrap();
        let p2 = s.send("a", "b", None, "ping 2", None, None).unwrap();
        let n = s.supersede_prior_idle("a", "b", p2).unwrap();
        assert_eq!(n, 1, "only the prior idle ping is superseded");
        assert_eq!(superseded_by_of(&s, "b", p1), Some(p2));
        assert_eq!(superseded_by_of(&s, "b", real), None);
        let unread = unread_ids(&s, "b");
        assert!(unread.contains(&real), "real message stays unread");
        assert!(unread.contains(&p2));
        assert!(!unread.contains(&p1));
    }

    #[test]
    fn idle_dedup_only_supersedes_unread_libsql() {
        let s = mem();
        let p1 = s.send("a", "b", None, "ping 1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", p1).unwrap();
        // b reads the first ping.
        let _ = s.inbox("b", false, true, 50).unwrap();
        let p2 = s.send("a", "b", None, "ping 2", None, None).unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", p2).unwrap(), 0);
        assert_eq!(superseded_by_of(&s, "b", p1), None);
    }

    #[test]
    fn idle_dedup_scoped_and_authz_self_only_libsql() {
        let s = mem();
        // a->b and c->b idle pings, plus a->z.
        let a_b1 = s.send("a", "b", None, "a1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", a_b1).unwrap();
        let c_b = s.send("c", "b", None, "c1", None, None).unwrap();
        s.supersede_prior_idle("c", "b", c_b).unwrap();
        let a_z = s.send("a", "z", None, "az1", None, None).unwrap();
        s.supersede_prior_idle("a", "z", a_z).unwrap();
        // a's new ping supersedes ONLY a's prior a->b ping.
        let a_b2 = s.send("a", "b", None, "a2", None, None).unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", a_b2).unwrap(), 1);
        assert_eq!(superseded_by_of(&s, "b", a_b1), Some(a_b2));
        assert_eq!(superseded_by_of(&s, "b", c_b), None);
        assert_eq!(superseded_by_of(&s, "z", a_z), None);
        // Authz: c cannot supersede a's prior ping (sender-scoped). A fresh c->b
        // ping deduping as 'c' leaves any a->b ping untouched.
        let a_b3 = s.send("a", "b", None, "a3", None, None).unwrap();
        s.supersede_prior_idle("a", "b", a_b3).unwrap();
        let c_b2 = s.send("c", "b", None, "c2", None, None).unwrap();
        s.supersede_prior_idle("c", "b", c_b2).unwrap();
        assert_eq!(
            superseded_by_of(&s, "b", a_b3),
            None,
            "c never touches a's ping"
        );
    }

    #[test]
    fn idle_dedup_rejects_missing_or_wrong_route_successor_libsql() {
        let s = mem();
        let prior = s.send("a", "b", None, "prior", None, None).unwrap();
        s.supersede_prior_idle("a", "b", prior).unwrap();
        assert!(s.supersede_prior_idle("a", "b", 999_999).is_err());
        let wrong_sender = s.send("c", "b", None, "wrong", None, None).unwrap();
        assert!(s.supersede_prior_idle("a", "b", wrong_sender).is_err());
        let wrong_recipient = s.send("a", "z", None, "wrong", None, None).unwrap();
        assert!(s.supersede_prior_idle("a", "b", wrong_recipient).is_err());
        assert_eq!(superseded_by_of(&s, "b", prior), None);
    }

    #[test]
    fn idle_dedup_idempotency_replay_is_noop_libsql() {
        let s = mem();
        // Mirror SQLite: idle classification and supersession are one keyed
        // request. An exact retry returns the accepted row without reapplying
        // the mutation or ever pointing the row at itself.
        let (p1, created) = s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "ping",
                Some("k-1"),
                None,
                Some("normal"),
                None,
                0,
                true,
            )
            .unwrap();
        assert!(created);
        let (replay, replay_created) = s
            .send_configured_idempotent(
                "a",
                "b",
                None,
                "ping",
                Some("k-1"),
                Some("trace:retry"),
                Some("normal"),
                None,
                0,
                true,
            )
            .unwrap();
        assert_eq!(replay, p1, "idempotency replay returns the existing id");
        assert!(!replay_created, "idempotency replay must be a no-op");
        assert_eq!(
            superseded_by_of(&s, "b", p1),
            None,
            "must not self-supersede"
        );
        assert_eq!(s.unread_count("b").unwrap(), 1);
        assert!(
            s.send_configured_idempotent(
                "a",
                "b",
                None,
                "ping",
                Some("k-1"),
                None,
                Some("normal"),
                None,
                0,
                false,
            )
            .is_err(),
            "changing the keyed idle request must not alias the accepted message"
        );
    }

    // ---- WL-035: snapshot_to on the local libsql backend -------------------

    #[test]
    fn snapshot_to_roundtrips_messages_libsql() {
        let s = mem();
        s.send("a", "b", Some("s"), "hi", None, None).unwrap();
        s.send("a", "b", None, "again", None, None).unwrap();
        let src_count = s.total_messages().unwrap();
        assert_eq!(src_count, 2);
        let dir = std::env::temp_dir().join(format!("weave-libsql-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("snapshot.db");
        let _ = std::fs::remove_file(&dest);
        s.snapshot_to(&dest).unwrap();
        // The local VACUUM INTO snapshot opens read-only with the same count.
        let snap = LibsqlStore::open_readonly(&dest).unwrap();
        assert_eq!(snap.total_messages().unwrap(), src_count);
    }

    // ---- WL-038: ephemeral messages with TTL + auto-sweep (libsql mirror) ----

    #[test]
    fn expiry_stamps_and_excludes_from_unread_libsql() {
        let s = mem();
        let live = s
            .send("a", "b", Some("keep"), "permanent", None, None)
            .unwrap();
        let eph = s
            .send("a", "b", Some("v"), "ephemeral", None, None)
            .unwrap();
        s.set_message_expiry(eph, now() - 1).unwrap();
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(inbox.iter().any(|m| m.id == live));
        assert!(!inbox.iter().any(|m| m.id == eph));
        // Delete-on-sweep (history opportunistically sweeps).
        assert!(s
            .history("b", None, 100)
            .unwrap()
            .iter()
            .all(|m| m.id != eph));
        assert_eq!(s.total_messages().unwrap(), 1);
        let oldest = s.peek_oldest_unread("b").unwrap().unwrap();
        assert_eq!(oldest.id, live);
    }

    #[test]
    fn sweep_expired_messages_deletes_expired_keeps_live_libsql() {
        let s = mem();
        let expired = s.send("a", "b", None, "gone", None, None).unwrap();
        let future = s.send("a", "b", None, "soon", None, None).unwrap();
        let permanent = s.send("a", "b", None, "forever", None, None).unwrap();
        s.set_message_expiry(expired, now() - 5).unwrap();
        s.set_message_expiry(future, now() + 10_000).unwrap();
        let n = s.sweep_expired_messages().unwrap();
        assert_eq!(n, 1);
        let hist = s.history("b", None, 100).unwrap();
        assert!(hist.iter().any(|m| m.id == future));
        assert!(hist.iter().any(|m| m.id == permanent));
        assert!(hist.iter().all(|m| m.id != expired));
    }

    #[test]
    fn gc_also_reaps_expired_ephemeral_libsql() {
        let s = mem();
        let eph = s
            .send("a", "b", None, "fresh-but-expired", None, None)
            .unwrap();
        s.set_message_expiry(eph, now() - 1).unwrap();
        s.gc(86_400 * 365).unwrap();
        assert_eq!(s.total_messages().unwrap(), 0);
    }

    #[test]
    fn non_ephemeral_message_is_never_swept_libsql() {
        let s = mem();
        let mid = s.send("a", "b", None, "forever", None, None).unwrap();
        assert_eq!(s.sweep_expired_messages().unwrap(), 0);
        s.gc(86_400 * 365).unwrap();
        let hist = s.history("b", None, 100).unwrap();
        assert!(hist.iter().any(|m| m.id == mid));
    }

    #[test]
    fn cross_store_intent_carries_ttl_to_expiry_libsql() {
        let dir =
            std::env::temp_dir().join(format!("weave-libsql-ttl-xstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");
        let _ = std::fs::remove_file(&a_path);
        let _ = std::fs::remove_file(&b_path);
        let cfg_a = Config {
            db: Some(a_path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let cfg_b = Config {
            db: Some(b_path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let a = LibsqlStore::open(&cfg_a).unwrap();
        a.enqueue_intent("bob", "", "alice", None, "hi", "", None, None, None, 600)
            .unwrap();
        let b = LibsqlStore::open(&cfg_b).unwrap();
        let allow = vec![StoreSource::Local(a_path.clone())];
        let pulled = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(pulled.committed, 1);
        let hist = b.history("bob", None, 100).unwrap();
        let m = hist.iter().find(|m| m.body == "hi").expect("committed");
        let exp = m.expires_at.expect("ttl re-stamped as expiry");
        assert!(exp > now() + 500 && exp <= now() + 600);
    }

    #[test]
    fn unread_count_is_exact_validated_and_pure_on_readonly_libsql() {
        let s = mem();
        let path = s.local_path.clone().unwrap();
        let direct = s.send("alice", "bob", None, "direct", None, None).unwrap();
        s.send("alice", "all", None, "broadcast", None, None)
            .unwrap();
        s.send("bob", "bob", None, "self", None, None).unwrap();
        s.send("alice", "carol", None, "foreign", None, None)
            .unwrap();
        let old = s.send("alice", "bob", None, "old", None, None).unwrap();
        let new = s.send("alice", "bob", None, "new", None, None).unwrap();
        s.supersede("alice", old, new).unwrap();
        let expired = s.send("alice", "bob", None, "expired", None, None).unwrap();
        s.set_message_expiry(expired, now() - 1).unwrap();
        assert_eq!(Store::unread_count(&s, "bob").unwrap(), 3);
        assert!(Store::unread_count(&s, "").is_err());
        s.mark_message_read("bob", direct).unwrap();
        let rows_before = s.total_messages().unwrap();
        drop(s);

        let readonly = LibsqlStore::open_readonly(&path).unwrap();
        assert_eq!(Store::unread_count(&readonly, "bob").unwrap(), 2);
        assert_eq!(
            readonly.total_messages().unwrap(),
            rows_before,
            "pure count must not sweep the expired row"
        );
    }

    #[test]
    fn mark_message_read_is_exact_recipient_scoped_and_idempotent_libsql() {
        let s = mem();
        let direct = s.send("alice", "bob", None, "direct", None, None).unwrap();
        let broadcast = s
            .send("alice", "all", None, "broadcast", None, None)
            .unwrap();
        let foreign = s
            .send("alice", "carol", None, "foreign", None, None)
            .unwrap();
        let self_sent = s.send("bob", "bob", None, "self", None, None).unwrap();
        let superseded = s.send("alice", "bob", None, "old", None, None).unwrap();
        let successor = s.send("alice", "bob", None, "new", None, None).unwrap();
        s.supersede("alice", superseded, successor).unwrap();
        let expired = s.send("alice", "bob", None, "expired", None, None).unwrap();
        s.set_message_expiry(expired, now() - 1).unwrap();

        assert!(s.mark_message_read("bob", direct).unwrap());
        assert!(s.mark_message_read("bob", direct).unwrap());
        assert!(s.mark_message_read("bob", broadcast).unwrap());
        assert!(!s.mark_message_read("bob", foreign).unwrap());
        assert!(!s.mark_message_read("bob", self_sent).unwrap());
        assert!(!s.mark_message_read("bob", superseded).unwrap());
        assert!(!s.mark_message_read("bob", expired).unwrap());
        assert!(!s.mark_message_read("bob", i64::MAX).unwrap());
        assert!(!s.mark_message_read("bob", -1).unwrap());
        assert!(s.mark_message_read("", direct).is_err());
        assert_eq!(s.receipts(direct).unwrap().len(), 1);
        assert_eq!(s.receipts(broadcast).unwrap().len(), 1);
        assert!(s.receipts(successor).unwrap().is_empty());
        assert!(s.receipts(foreign).unwrap().is_empty());
    }

    #[test]
    fn bridge_inbox_completion_is_atomic_fenced_and_restart_durable_libsql() {
        let s = mem();
        let path = s.local_path.clone().unwrap();
        s.claim_bridge_runtime(
            BridgePlatform::Telegram,
            "telegram-bridge",
            "operator",
            "owner-a",
            Some(10),
            "host-a",
            0,
        )
        .unwrap()
        .unwrap();
        let first = s
            .send("alice", "telegram-bridge", None, "first", None, None)
            .unwrap();
        let second = s
            .send("carol", "telegram-bridge", None, "second", None, None)
            .unwrap();
        let foreign = s
            .send("alice", "someone-else", None, "foreign", None, None)
            .unwrap();
        let update = BridgeRuntimeUpdate {
            cursor: Some("telegram-cursor-9".into()),
            ..BridgeRuntimeUpdate::default()
        };

        assert!(s
            .complete_bridge_inbox_snapshot(
                BridgePlatform::Telegram,
                "owner-a",
                "telegram-bridge",
                &[first, foreign],
                &update,
            )
            .is_err());
        assert!(s.receipts(first).unwrap().is_empty());
        assert_eq!(
            s.bridge_runtime_status(BridgePlatform::Telegram)
                .unwrap()
                .unwrap()
                .cursor,
            ""
        );
        assert_eq!(s.unread_count("telegram-bridge").unwrap(), 2);

        assert!(!s
            .complete_bridge_inbox_snapshot(
                BridgePlatform::Telegram,
                "stale-owner",
                "telegram-bridge",
                &[first, second],
                &update,
            )
            .unwrap());
        assert!(s.receipts(first).unwrap().is_empty());

        assert!(s
            .complete_bridge_inbox_snapshot(
                BridgePlatform::Telegram,
                "owner-a",
                "telegram-bridge",
                &[first, second],
                &update,
            )
            .unwrap());
        assert_eq!(s.unread_count("telegram-bridge").unwrap(), 0);
        drop(s);

        let reopened = LibsqlStore::open(&Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".into()),
            ..Config::default()
        })
        .unwrap();
        assert_eq!(
            reopened
                .bridge_runtime_status(BridgePlatform::Telegram)
                .unwrap()
                .unwrap()
                .cursor,
            "telegram-cursor-9"
        );
        assert_eq!(reopened.unread_count("telegram-bridge").unwrap(), 0);
        assert!(reopened
            .complete_bridge_inbox_snapshot(
                BridgePlatform::Telegram,
                "owner-a",
                "telegram-bridge",
                &[first, second],
                &update,
            )
            .unwrap());
        assert_eq!(reopened.receipts(first).unwrap().len(), 1);
        assert_eq!(reopened.receipts(second).unwrap().len(), 1);
    }

    #[test]
    fn bridge_runtime_claim_fencing_cursor_reclaim_release_and_bounds_libsql() {
        let s = mem();
        let fresh_cutoff = now().saturating_sub(60);
        let claimed = s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner-a",
                Some(10),
                "host-a",
                fresh_cutoff,
            )
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, BridgeRuntimeStatus::Starting);
        assert!(s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner-b",
                Some(11),
                "host-b",
                fresh_cutoff,
            )
            .unwrap()
            .is_none());
        assert!(s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner-a",
                Some(10),
                "host-a",
                fresh_cutoff,
            )
            .unwrap()
            .is_some());
        let update = BridgeRuntimeUpdate {
            cursor: Some("cursor-101".into()),
            status: Some(BridgeRuntimeStatus::Running),
            last_poll_ts: Some(100),
            last_success_ts: Some(90),
            last_delivery_ts: Some(80),
            error: BridgeRuntimeErrorUpdate::Set {
                class: "provider_timeout".into(),
                message: "bounded detail".into(),
            },
        };
        assert!(!s
            .update_bridge_runtime(BridgePlatform::Telegram, "owner-b", &update)
            .unwrap());
        assert!(s
            .update_bridge_runtime(BridgePlatform::Telegram, "owner-a", &update)
            .unwrap());
        assert!(s
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                "owner-a",
                &BridgeRuntimeUpdate {
                    last_poll_ts: Some(50),
                    last_success_ts: Some(40),
                    last_delivery_ts: Some(30),
                    error: BridgeRuntimeErrorUpdate::Clear,
                    ..BridgeRuntimeUpdate::default()
                },
            )
            .unwrap());
        let state = s
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .unwrap();
        assert_eq!(state.cursor, "cursor-101");
        assert_eq!(state.status, BridgeRuntimeStatus::Running);
        assert_eq!(
            (
                state.last_poll_ts,
                state.last_success_ts,
                state.last_delivery_ts
            ),
            (100, 90, 80)
        );
        assert!(state.last_error_class.is_empty() && state.last_error.is_empty());

        s.claim_bridge_runtime(
            BridgePlatform::Slack,
            "slack-bridge",
            "operator",
            "slack-owner",
            Some(12),
            "host-s",
            fresh_cutoff,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s.list_bridge_runtime_statuses()
                .unwrap()
                .into_iter()
                .map(|state| state.platform)
                .collect::<Vec<_>>(),
            vec![BridgePlatform::Telegram, BridgePlatform::Slack]
        );

        assert!(s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner-b",
                Some(11),
                "host-b",
                now() + 1,
            )
            .unwrap()
            .is_some());
        assert!(!s
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                "owner-a",
                &BridgeRuntimeUpdate::default(),
            )
            .unwrap());
        assert!(!s
            .release_bridge_runtime(BridgePlatform::Telegram, "owner-a")
            .unwrap());
        assert!(s
            .release_bridge_runtime(BridgePlatform::Telegram, "owner-b")
            .unwrap());
        let released = s
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .unwrap();
        assert_eq!(released.status, BridgeRuntimeStatus::Stopped);
        assert!(released.owner_id.is_empty());
        assert_eq!(released.cursor, "cursor-101");

        assert!(s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                &"x".repeat(crate::model::MAX_BRIDGE_OWNER_ID_LEN + 1),
                Some(1),
                "host",
                0,
            )
            .is_err());
        assert!(s
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner",
                Some(0),
                "host",
                0,
            )
            .is_err());
        assert!(s
            .update_bridge_runtime(
                BridgePlatform::Slack,
                "slack-owner",
                &BridgeRuntimeUpdate {
                    cursor: Some("x".repeat(crate::model::MAX_BRIDGE_CURSOR_LEN + 1)),
                    ..BridgeRuntimeUpdate::default()
                },
            )
            .is_err());
        assert!(s
            .update_bridge_runtime(
                BridgePlatform::Slack,
                "slack-owner",
                &BridgeRuntimeUpdate {
                    status: Some(BridgeRuntimeStatus::Stopped),
                    ..BridgeRuntimeUpdate::default()
                },
            )
            .is_err());
        assert!(s
            .update_bridge_runtime(
                BridgePlatform::Slack,
                "slack-owner",
                &BridgeRuntimeUpdate {
                    error: BridgeRuntimeErrorUpdate::Set {
                        class: "x".repeat(crate::model::MAX_BRIDGE_ERROR_CLASS_LEN + 1),
                        message: String::new(),
                    },
                    ..BridgeRuntimeUpdate::default()
                },
            )
            .is_err());
    }

    #[test]
    fn concurrent_bridge_runtime_claim_collision_has_exactly_one_winner_libsql() {
        use std::sync::{Arc, Barrier};

        let dir = std::env::temp_dir().join(format!(
            "weave-libsql-bridge-claim-race-{}-{}",
            std::process::id(),
            new_attempt_id(now())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let open = |path: &std::path::Path| {
            LibsqlStore::open(&Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".into()),
                ..Config::default()
            })
            .unwrap()
        };
        drop(open(&path));

        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["owner-a", "owner-b"]
            .into_iter()
            .map(|owner| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let store = LibsqlStore::open(&Config {
                        db: Some(path.to_string_lossy().into_owned()),
                        backend: Some("libsql".into()),
                        ..Config::default()
                    })
                    .unwrap();
                    barrier.wait();
                    store.claim_bridge_runtime(
                        BridgePlatform::Telegram,
                        "telegram-bridge",
                        "operator",
                        owner,
                        Some(1),
                        "host",
                        now().saturating_sub(60),
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|state| state.is_some()).count(), 1);
        let winner = open(&path)
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .unwrap();
        assert!(matches!(winner.owner_id.as_str(), "owner-a" | "owner-b"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bridge_runtime_table_migrates_on_legacy_libsql_open() {
        let s = mem();
        let path = s.local_path.clone().unwrap();
        s.rt.block_on(async {
            s.conn
                .execute("DROP TABLE bridge_runtime", ())
                .await
                .unwrap();
            s.conn
                .execute("DROP TABLE bridge_staged_events", ())
                .await
                .unwrap();
            s.conn
                .execute("DROP INDEX idx_delivery_log_exact", ())
                .await
                .unwrap();
        });
        drop(s);
        let cfg = Config {
            db: Some(path.to_string_lossy().into_owned()),
            backend: Some("libsql".to_string()),
            ..Config::default()
        };
        let reopened = LibsqlStore::open(&cfg).unwrap();
        assert!(reopened
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .is_none());
        assert!(reopened
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "operator",
                "owner",
                Some(1),
                "host",
                0,
            )
            .unwrap()
            .is_some());
        let ddl = reopened
            .rt
            .block_on(async {
                let mut rows = reopened
                    .conn
                    .query(
                        "SELECT sql FROM sqlite_master WHERE type='table' AND name='bridge_runtime'",
                        (),
                    )
                    .await?;
                let row = rows.next().await?.expect("bridge_runtime ddl");
                Ok::<_, anyhow::Error>(row.get::<String>(0)?)
            })
            .unwrap();
        assert!(!ddl.to_ascii_lowercase().contains("token"));
        let exact_index = reopened
            .rt
            .block_on(async {
                let mut rows = reopened
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_delivery_log_exact'",
                        (),
                    )
                    .await?;
                let row = rows.next().await?.expect("exact relay lookup index");
                Ok::<_, anyhow::Error>(row.get::<i64>(0)?)
            })
            .unwrap();
        assert_eq!(
            exact_index, 1,
            "legacy opens add the exact relay lookup index"
        );
        let staging_schema = reopened
            .rt
            .block_on(async {
                let mut rows = reopened
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM sqlite_master
                          WHERE (type='table' AND name='bridge_staged_events')
                             OR (type='index' AND name='idx_bridge_staged_route_order')",
                        (),
                    )
                    .await?;
                let row = rows.next().await?.expect("durable bridge staging schema");
                Ok::<_, anyhow::Error>(row.get::<i64>(0)?)
            })
            .unwrap();
        assert_eq!(staging_schema, 2, "legacy opens add durable bridge staging");
    }
}
