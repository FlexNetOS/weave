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
    AskRole, AskState, ClaimOutcome, DeliveryTrace, Intent, Job, JobFilter, JobPatch,
    JobResultView, JobSpec, JobState, Lease, Message, OrchestratorStatus, Peer, PermissionStatus,
    ReviewItem, ReviewItemState, ReviewQueueFilter, Schedule, ScheduleKind, BROADCAST_SQL,
    MAX_CRON_EXPR_LEN, MAX_DELIVERY_ROWS, MAX_REVIEW_IDENT_LEN, MAX_REVIEW_TITLE_LEN,
};
use crate::store::{
    append_progress_event, canonical_source, check_birth_cert, check_body, check_host, check_ident,
    check_job_text, clamp_field, clamp_limit, commit_pulled, is_alive, job_result_view,
    merge_peer_views, merge_session_views, mint_birth_cert, remote_scheme_host, reply_subject,
    sanitize_tag, store_label, validate_job_patch, validate_job_spec, AskManyOutcome, Origin,
    PeerView, Pulled, RevocationEvent, RevocationKind, SessionInfo, SessionView, Store,
    VerifyPolicy, MAX_ASK_MANY_TARGETS, MAX_BRANCH_LEN, MAX_KEYS_PER_IDENT, MAX_PULL_PER_DRAIN,
    MAX_REPO_LEN, MAX_REVOCATIONS_LIST, MAX_SESSIONS, MAX_WORKTREE_LEN, PRESENCE_TTL_SECS,
};
use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database, OpenFlags, Value};
use tokio::runtime::Runtime;

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
        superseded_by   INTEGER
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
        contact_policy TEXT NOT NULL DEFAULT 'open'
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
        priority        TEXT NOT NULL DEFAULT 'normal'
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
    "CREATE INDEX IF NOT EXISTS idx_delivery_log_ts ON delivery_log(ts)",
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
        refreshed_ts INTEGER NOT NULL
    )",
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
    })
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

        let url = cfg.libsql_url.clone();
        let token = cfg.libsql_auth_token.clone();
        let path = cfg.db_path();
        // WL-035: a local-file backend (no remote URL) snapshots from this path;
        // a remote backend has no local file (`None`).
        let local_path = if cfg.libsql_url.is_none() {
            Some(path.clone())
        } else {
            None
        };

        let (db, conn) = rt.block_on(async move {
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
                "CREATE INDEX IF NOT EXISTS idx_delivery_log_ts ON delivery_log(ts)",
            ] {
                conn.execute(idx, ())
                    .await
                    .context("creating delivery_log index")?;
            }
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
            ] {
                let probe = format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='{col}'");
                let mut it = conn.query(&probe, ()).await?;
                if it.next().await?.is_none() {
                    conn.execute(ddl, ()).await.with_context(|| format!("adding {table}.{col} column"))?;
                }
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
                    refreshed_ts INTEGER NOT NULL
                )",
                (),
            )
            .await
            .context("creating summaries table")?;
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
        })?;

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
        let since = local.pull_cursor_get(&source)?;
        let intents = match foreign.list_outbox(me, since, MAX_PULL_PER_DRAIN) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[weave] skipping pull source '{label}': {e}");
                out.sources_skipped += 1;
                continue;
            }
        };
        let n = commit_pulled(local, me, &source, policy, intents)?;
        out.committed += n;
        if n > 0 {
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

    fn send(
        &self,
        sender: &str,
        recipient: &str,
        subject: Option<&str>,
        body: &str,
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<i64> {
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_ident("recipient", recipient)?;
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
        self.rt.block_on(async {
            if let Some(key) = idempotency_key {
                let mut it = self
                    .conn
                    .query(
                        "SELECT id FROM messages WHERE idempotency_key = ?1",
                        params(vec![key.into()]),
                    )
                    .await?;
                if let Some(r) = it.next().await? {
                    return Ok(r.get::<i64>(0)?);
                }
            }
            self.conn
                .execute(
                    "INSERT INTO messages (ts, sender, recipient, subject, body, idempotency_key, trace_id) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params(vec![
                        now().into(),
                        sender.into(),
                        recipient.into(),
                        subject.map(|s| s.to_string()).into(),
                        body.into(),
                        idempotency_key.map(|s| s.to_string()).into(),
                        trace_id.map(|s| s.to_string()).into(),
                    ]),
                )
                .await?;
            Ok(self.conn.last_insert_rowid())
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
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let sql = if include_read {
                format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages
                     WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                       AND superseded_by IS NULL
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                )
            } else {
                format!(
                    "SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to, m.idempotency_key, m.trace_id, m.priority, m.superseded_by FROM messages m
                     WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
                       AND m.superseded_by IS NULL
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

            let mut rows_iter = tx.query(&sql, params(vec![me.into(), limit.into()])).await?;
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

    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let mut rows: Vec<Message> = Vec::new();
            if let Some(p) = peer {
                let sql = format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages
                     WHERE (sender = ?1 AND (recipient = ?2 OR recipient IN {bc}))
                        OR (sender = ?2 AND (recipient = ?1 OR recipient IN {bc}))
                     ORDER BY id DESC LIMIT ?3",
                    bc = BROADCAST_SQL
                );
                let mut it = self
                    .conn
                    .query(&sql, params(vec![me.into(), p.into(), limit.into()]))
                    .await?;
                while let Some(r) = it.next().await? {
                    rows.push(row_to_message(&r)?);
                }
            } else {
                let sql = format!(
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages
                     WHERE sender = ?1 OR recipient = ?1 OR recipient IN {bc}
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                );
                let mut it = self
                    .conn
                    .query(&sql, params(vec![me.into(), limit.into()]))
                    .await?;
                while let Some(r) = it.next().await? {
                    rows.push(row_to_message(&r)?);
                }
            };
            rows.reverse();
            Ok(rows)
        })
    }

    fn search(&self, query: &str, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let sql = "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages
                 WHERE id IN (
                     SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2
                 )
                 ORDER BY id DESC LIMIT ?2";
            let mut it = self
                .conn
                .query(sql, params(vec![query.into(), limit.into()]))
                .await?;
            let mut rows: Vec<Message> = Vec::new();
            while let Some(r) = it.next().await? {
                rows.push(row_to_message(&r)?);
            }
            Ok(rows)
        })
    }

    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let sql = format!(
                "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages
                 WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1 AND id > ?2
                   AND superseded_by IS NULL
                 ORDER BY id ASC LIMIT ?3",
                bc = BROADCAST_SQL
            );
            let mut it = self
                .conn
                .query(&sql, params(vec![me.into(), since_id.into(), limit.into()]))
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
            tx.execute("DELETE FROM messages", ()).await?;
            tx.execute("DELETE FROM reads", ()).await?;
            tx.execute("DELETE FROM wake_acks", ()).await?;
            tx.commit().await?;
            Ok(n)
        })
    }

    fn peek_oldest_unread(&self, me: &str) -> Result<Option<Message>> {
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
            tx.execute(
                "DELETE FROM reads WHERE message_id IN (SELECT id FROM messages WHERE ts < ?1)",
                params(vec![cutoff.into()]),
            )
            .await?;
            tx.execute(
                "DELETE FROM messages WHERE ts < ?1",
                params(vec![cutoff.into()]),
            )
            .await?;
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

    fn reply(&self, sender: &str, in_reply_to: i64, body: &str) -> Result<i64> {
        // Atomic override of the trait default (which does send() +
        // set_in_reply_to() in two round-trips): resolve the parent and INSERT a
        // single row carrying in_reply_to, all inside ONE IMMEDIATE transaction
        // so the parent cannot vanish mid-reply. Mirrors SqliteStore::reply.
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_body(body)?;
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;

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
                (recipient, reply_subject(psubject.as_deref()))
            };

            tx.execute(
                "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params(vec![
                    now().into(),
                    sender.into(),
                    recipient.into(),
                    subject.into(),
                    body.into(),
                    in_reply_to.into(),
                ]),
            )
            .await?;
            let id = self.conn.last_insert_rowid();
            tx.commit().await?;
            Ok(id)
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
                       m.idempotency_key, m.trace_id, m.priority, m.superseded_by
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
    ) -> Result<String> {
        self.guard_writable()?;
        check_ident("peer name", name)?;
        if let Some(cert) = birth_cert {
            check_birth_cert(cert)?;
        }
        let repo = sanitize_tag(repo, MAX_REPO_LEN);
        let branch = sanitize_tag(branch, MAX_BRANCH_LEN);
        let worktree_id = sanitize_tag(worktree_id, MAX_WORKTREE_LEN);
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
                        "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, birth_cert)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
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
                        ]),
                    )
                    .await?;
                    new_cert
                }
                Some(None) => {
                    let new_cert = mint_birth_cert()?;
                    tx.execute(
                        "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11, birth_cert=?12
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
                            new_cert.clone().into(),
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
                    tx.execute(
                        "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11
                         WHERE name=?12",
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
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE name=?1",
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
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers ORDER BY name",
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
                        "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE role='orchestrator'",
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
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE role='orchestrator'",
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

    fn enqueue_intent(
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
    ) -> Result<i64> {
        self.guard_writable()?;
        check_ident("recipient", to)?;
        check_ident("sender", from)?;
        check_host(to_host)?;
        check_body(body)?;
        let p = priority.unwrap_or("normal").to_string();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO outbox (ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
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
                    ]),
                )
                .await?;
            Ok(self.conn.last_insert_rowid())
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
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority FROM outbox \
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
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority FROM outbox \
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
                     ON CONFLICT(source) DO UPDATE SET last_id = ?2",
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

    fn ask(
        &self,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        body: &str,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(String, i64)> {
        self.guard_writable()?;
        check_ident("asker", asker)?;
        check_ident("askee", askee)?;
        check_body(body)?;
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
        let ts = now();
        let subject_owned = subject.map(|s| s.to_string());
        let reply_to_owned = reply_to.map(|s| s.to_string());
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;

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
                "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params(vec![
                    ts.into(),
                    asker.into(),
                    askee.into(),
                    subject_final.clone().into(),
                    body.into(),
                    in_reply_to.into(),
                ]),
            )
            .await?;
            let question_msg_id = self.conn.last_insert_rowid();
            let id = new_ask_id(question_msg_id);
            // A plain `ask` is never part of a group: parent_id is NULL. Ask-many
            // children share this insert shape with a non-NULL parent_id.
            tx.execute(
                "INSERT INTO asks \
                    (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, \
                     options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id) \
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10, NULL, NULL)",
                params(vec![
                    id.clone().into(),
                    question_msg_id.into(),
                    asker.into(),
                    askee.into(),
                    subject_final.into(),
                    AskState::Open.as_str().into(),
                    kind.as_str().into(),
                    options.into(),
                    reply_to_owned.clone().into(),
                    ts.into(),
                ]),
            )
            .await?;
            tx.commit().await?;
            Ok((id, question_msg_id))
        })
    }

    fn answer(&self, responder: &str, correlation_id: &str, body: &str) -> Result<i64> {
        self.guard_writable()?;
        check_ident("responder", responder)?;
        check_body(body)?;
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        let ts = now();
        self.rt.block_on(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            let (asker, askee, state, question_msg_id) = {
                let mut rows = tx
                    .query(
                        "SELECT asker, askee, state, question_msg_id FROM asks WHERE id = ?1",
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
                )
            };
            if responder != askee {
                anyhow::bail!("only the askee '{askee}' can answer ask '{correlation_id}'.");
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
                "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params(vec![
                    ts.into(),
                    responder.into(),
                    asker.into(),
                    subject.into(),
                    body.into(),
                    question_msg_id.into(),
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
            Ok(answer_msg_id)
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
        check_body(body)?;
        if is_broadcast(asker) {
            anyhow::bail!("the ask-many asker must be a concrete peer, not a broadcast alias.");
        }
        if let Some(s) = subject {
            check_body(s)?;
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

    fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()> {
        self.guard_writable()?;
        self.rt.block_on(async {
            // Both ids must exist; look up the predecessor's sender for the
            // authorization check.
            let mut it = self
                .conn
                .query(
                    "SELECT sender FROM messages WHERE id = ?1",
                    params(vec![old_id.into()]),
                )
                .await?;
            let old_sender = match it.next().await? {
                Some(r) => r.get::<String>(0)?,
                None => anyhow::bail!("cannot supersede: message #{old_id} does not exist"),
            };
            drop(it);
            let mut it = self
                .conn
                .query(
                    "SELECT 1 FROM messages WHERE id = ?1",
                    params(vec![new_id.into()]),
                )
                .await?;
            if it.next().await?.is_none() {
                anyhow::bail!("cannot supersede: successor message #{new_id} does not exist");
            }
            drop(it);
            // Authorization: only the ORIGINAL SENDER of old_id may supersede it
            // (best-effort same-identity guard; censorship/DoS protection).
            if old_sender != caller {
                anyhow::bail!(
                    "cannot supersede: #{old_id} was sent by '{old_sender}', not '{caller}'"
                );
            }
            self.conn
                .execute(
                    "UPDATE messages SET superseded_by = ?2 WHERE id = ?1",
                    params(vec![old_id.into(), new_id.into()]),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
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
        let ts = now();
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO summaries (root_id, text, model, created_ts, refreshed_ts)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(root_id) DO UPDATE SET
                     text = excluded.text,
                     model = excluded.model,
                     refreshed_ts = excluded.refreshed_ts",
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

    fn get_summary(&self, root_id: i64) -> Result<Option<crate::model::Summary>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT root_id, text, model, created_ts, refreshed_ts
                     FROM summaries WHERE root_id = ?1",
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
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
        bc = BROADCAST_SQL
    );
    let mut it = conn.query(&sql, params(vec![me.into()])).await?;
    match it.next().await? {
        Some(r) => Ok(r.get::<i64>(0)?),
        None => Ok(0),
    }
}

async fn peek_oldest_unread_on(conn: &Connection, me: &str) -> Result<Option<Message>> {
    let sql = format!(
        "SELECT id, ts, sender, recipient, subject, body, in_reply_to, idempotency_key, trace_id, priority, superseded_by FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND m.superseded_by IS NULL
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)
         ORDER BY m.id ASC LIMIT 1",
        bc = BROADCAST_SQL
    );
    let mut it = conn.query(&sql, params(vec![me.into()])).await?;
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
                "auto", "tmux", "%1", "", None, None, "h", "", "", "", "default", None,
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

    /// libsql parity (P2): a legacy P1-era DB whose `asks` lacks `parent_id` (and has
    /// no `ask_groups`) upgrades in place — parent_id added NULL on the old row,
    /// ask_groups created, a fresh fanout then works. Mirrors the sqlite test.
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
        // Seed a P1-era store, then drop parent_id + ask_groups to simulate the older
        // schema (recreate `asks` WITHOUT parent_id, drop ask_groups).
        {
            let cfg = Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let s = LibsqlStore::open(&cfg).unwrap();
            s.send("a", "b", None, "q", None, None).unwrap();
            s.rt
                .block_on(async {
                    s.conn.execute("DROP TABLE asks", ()).await?;
                    s.conn.execute("DROP TABLE ask_groups", ()).await?;
                    s.conn
                        .execute(
                            "CREATE TABLE asks (
                                id TEXT PRIMARY KEY, question_msg_id INTEGER NOT NULL,
                                answer_msg_id INTEGER, asker TEXT NOT NULL, askee TEXT NOT NULL,
                                subject TEXT, state TEXT NOT NULL, reply_to TEXT, close_note TEXT,
                                opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL,
                                closed_ts INTEGER)",
                            (),
                        )
                        .await?;
                    s.conn
                        .execute(
                            "INSERT INTO asks (id, question_msg_id, asker, askee, state, opened_ts, updated_ts) \
                             VALUES ('ask_1_legacy', 1, 'a', 'b', 'open', 1, 1)",
                            (),
                        )
                        .await?;
                    Ok::<_, anyhow::Error>(())
                })
                .unwrap();
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
                "p", "tmux", "%1", "", None, None, "h", "", "", "", "team-a", None,
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
            "a", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None,
        )
        .unwrap();
        s.register_peer_full(
            "b", "tmux", "%2", "", None, None, "h", "", "", "", "c1", None,
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
            "o", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None,
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
        )
        .unwrap();
        let local = s.get_peer("local").unwrap().unwrap();
        assert_eq!(liveness_for(&local, &this, now_ts), Liveness::AliveLocal);

        // same-host + null pid + recent => AliveLocal (TTL fallback).
        s.register_peer_full(
            "nullpid", "tmux", "%2", "", None, None, &this, "", "", "", "default", None,
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
            "intruder", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None,
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
                "intruder", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None,
            )
            .map(|_| ()),
        );
        assert_trapped(
            "enqueue_intent",
            ro.enqueue_intent("to", "boxB", "from", None, "body", "", None, None, None)
                .map(|_| ()),
        );
        assert_trapped("pull_cursor_set", ro.pull_cursor_set("src", 5));
        assert_trapped("register_key", ro.register_key("id", "pubkey"));

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
                "me", "tmux", "%1", "", None, None, "boxA", "", "", "", "default", None,
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
                    "them", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None,
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
        )
        .unwrap();
        let i3 = s
            .enqueue_intent("bob", "", "alice", None, "body3", "", None, None, None)
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
        assert!(s.get_summary(1).unwrap().is_none());
        s.store_summary(1, "summary text", "gpt-4").unwrap();
        let sum = s.get_summary(1).unwrap().unwrap();
        assert_eq!(sum.root_id, 1);
        assert_eq!(sum.text, "summary text");
        assert_eq!(sum.model, "gpt-4");
        // Upsert refreshes
        s.store_summary(1, "new text", "gpt-3").unwrap();
        let sum2 = s.get_summary(1).unwrap().unwrap();
        assert_eq!(sum2.text, "new text");
        assert_eq!(sum2.model, "gpt-3");
        assert!(s.delete_summary(1).unwrap());
        assert!(!s.delete_summary(1).unwrap());
        assert!(s.get_summary(1).unwrap().is_none());
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
        let b = s.send("c", "b", None, "C", None, None).unwrap();
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
}
