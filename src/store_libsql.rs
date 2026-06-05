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
use crate::model::{is_broadcast, now, Intent, Message, Peer, BROADCAST_SQL};
use crate::store::{
    canonical_source, check_body, check_host, check_ident, clamp_limit, commit_pulled,
    merge_peer_views, merge_session_views, remote_scheme_host, reply_subject, sanitize_tag,
    store_label, Origin, PeerView, Pulled, SessionInfo, SessionView, Store, MAX_BRANCH_LEN,
    MAX_PULL_PER_DRAIN, MAX_REPO_LEN, MAX_SESSIONS, MAX_WORKTREE_LEN,
};
use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database, OpenFlags, Value};
use tokio::runtime::Runtime;

/// Same schema as `SqliteStore`. Executed statement-by-statement because the
/// libsql remote/HTTP path runs one statement per round-trip; `execute_batch`
/// works for local but splitting keeps both backends identical.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS messages (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        ts          INTEGER NOT NULL,
        sender      TEXT NOT NULL,
        recipient   TEXT NOT NULL,
        subject     TEXT,
        body        TEXT NOT NULL,
        in_reply_to INTEGER
    )",
    "CREATE TABLE IF NOT EXISTS reads (
        message_id INTEGER NOT NULL,
        reader     TEXT NOT NULL,
        ts         INTEGER NOT NULL,
        PRIMARY KEY (message_id, reader)
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
        worktree_id TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS outbox (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        ts        INTEGER NOT NULL,
        to_peer   TEXT NOT NULL,
        to_host   TEXT NOT NULL DEFAULT '',
        from_peer TEXT NOT NULL,
        subject   TEXT,
        body      TEXT NOT NULL,
        sig       TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS pull_cursor (
        source  TEXT PRIMARY KEY,
        last_id INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS keys (
        identity TEXT PRIMARY KEY,
        pubkey   TEXT NOT NULL
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
    })
}

/// Column order: name, mux, target, socket, cwd, last_seen, pid, host, repo,
/// branch, worktree_id.
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
/// `strict` (`Config::strict_verify`, 2d) is forwarded to `commit_pulled`: under it
/// an unsigned/unverifiable intent is dropped rather than committed. Inert without
/// the `sign` feature.
pub fn pull_from_store(
    local: &dyn Store,
    me: &str,
    allow: &[StoreSource],
    strict: bool,
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
        let n = commit_pulled(local, me, &source, strict, intents)?;
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
    ) -> Result<i64> {
        self.guard_writable()?;
        check_ident("sender", sender)?;
        check_ident("recipient", recipient)?;
        check_body(body)?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO messages (ts, sender, recipient, subject, body) \
                     VALUES (?1,?2,?3,?4,?5)",
                    params(vec![
                        now().into(),
                        sender.into(),
                        recipient.into(),
                        subject.map(|s| s.to_string()).into(),
                        body.into(),
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
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to FROM messages
                     WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                )
            } else {
                format!(
                    "SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to FROM messages m
                     WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
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
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to FROM messages
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
                    "SELECT id, ts, sender, recipient, subject, body, in_reply_to FROM messages
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

    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        self.rt.block_on(async {
            let sql = format!(
                "SELECT id, ts, sender, recipient, subject, body, in_reply_to FROM messages
                 WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1 AND id > ?2
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
            tx.commit().await?;
            Ok(n)
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
                SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to
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
    ) -> Result<()> {
        self.guard_writable()?;
        check_ident("peer name", name)?;
        // Bound + control-strip the descriptive git tags at the store seam
        // (lossy-but-total), mirroring the sqlite backend.
        let repo = sanitize_tag(repo, MAX_REPO_LEN);
        let branch = sanitize_tag(branch, MAX_BRANCH_LEN);
        let worktree_id = sanitize_tag(worktree_id, MAX_WORKTREE_LEN);
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                     ON CONFLICT(name) DO UPDATE SET mux=?2, target=?3, socket=?4, cwd=?5, last_seen=?6, pid=?7, host=?8, repo=?9, branch=?10, worktree_id=?11",
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
                    ]),
                )
                .await?;
            Ok(())
        })
    }

    fn get_peer(&self, name: &str) -> Result<Option<Peer>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id FROM peers WHERE name=?1",
                    params(vec![name.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(row_to_peer(&r)?)),
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
                    "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id FROM peers ORDER BY name",
                    (),
                )
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push(row_to_peer(&r)?);
            }
            Ok(out)
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
    ) -> Result<i64> {
        self.guard_writable()?;
        check_ident("recipient", to)?;
        check_ident("sender", from)?;
        check_host(to_host)?;
        check_body(body)?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO outbox (ts, to_peer, to_host, from_peer, subject, body, sig) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params(vec![
                        now().into(),
                        to.into(),
                        to_host.into(),
                        from.into(),
                        subject.map(|s| s.to_string()).into(),
                        body.into(),
                        sig.into(),
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
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig FROM outbox \
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
                    "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig FROM outbox \
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
            self.conn
                .execute(
                    "INSERT INTO keys (identity, pubkey) VALUES (?1, ?2) \
                     ON CONFLICT(identity) DO UPDATE SET pubkey = ?2",
                    params(vec![identity.into(), pubkey.into()]),
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
                    "SELECT pubkey FROM keys WHERE identity = ?1",
                    params(vec![identity.into()]),
                )
                .await?;
            match it.next().await? {
                Some(r) => Ok(Some(r.get::<String>(0)?)),
                None => Ok(None),
            }
        })
    }

    fn list_keys(&self) -> Result<Vec<(String, String)>> {
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query("SELECT identity, pubkey FROM keys ORDER BY identity", ())
                .await?;
            let mut out = Vec::new();
            while let Some(r) = it.next().await? {
                out.push((r.get::<String>(0)?, r.get::<String>(1)?));
            }
            Ok(out)
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
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
        bc = BROADCAST_SQL
    );
    let mut it = conn.query(&sql, params(vec![me.into()])).await?;
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
        clamp_limit, is_alive, is_online, pid_alive, MAX_IDENT, MAX_LIMIT, ONLINE_TTL_SECS,
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
    fn send_and_read_tracking() {
        let s = mem();
        s.send("desktop", "envctl", Some("hi"), "body1").unwrap();
        s.send("desktop", "all", None, "bcast").unwrap();

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
        s.send("a", "b", None, "1").unwrap();
        s.send("b", "a", None, "2").unwrap();
        s.send("c", "d", None, "x").unwrap();
        let h = s.history("a", Some("b"), 50).unwrap();
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn reply_addresses_back_and_links() {
        let s = mem();
        let root = s.send("a", "b", Some("hi"), "question?").unwrap();
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
        let root = s.send("a", "b", Some("topic"), "m0").unwrap();
        let c1 = s.reply("b", root, "m1").unwrap();
        let c2 = s.reply("a", c1, "m2").unwrap(); // nested reply-to-a-reply
        let _other = s.send("a", "b", None, "unrelated").unwrap();

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
        let id = s.send("a", "all", None, "ping").unwrap();
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
        let id1 = s.send("a", "b", None, "m1").unwrap();
        let id2 = s.send("a", "b", None, "m2").unwrap();
        let id3 = s.send("a", "all", None, "bcast").unwrap();

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
        let id_old = s.send("a", "b", None, "old").unwrap();
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
        s.send("a", "b", None, "new").unwrap();
        let deleted = s.gc(3600).unwrap(); // older than 1h
        assert_eq!(deleted, 1);
        assert_eq!(s.total_messages().unwrap(), 1);
        let (rows, _) = s.inbox("b", true, false, 50).unwrap();
        assert_eq!(rows[0].body, "new");
    }

    #[test]
    fn negative_limit_is_not_unbounded() {
        let s = mem();
        for i in 0..5 {
            s.send("a", "b", None, &format!("m{i}")).unwrap();
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
        s.send("a", "b", None, "1").unwrap();
        s.send("a", "b", None, "2").unwrap();
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
        assert!(s.send("", "b", None, "x").is_err(), "empty sender rejected");
        assert!(
            s.send("a", "", None, "x").is_err(),
            "empty recipient rejected"
        );
        assert!(
            s.send("a", "b\nc", None, "x").is_err(),
            "control char in recipient rejected"
        );
        assert!(s.send("a", "b", None, "x").is_ok());
    }

    // ---- A2 (real liveness): mirror of the SqliteStore store-unit tests ----

    /// `register_peer_full` round-trips the new `pid`/`host` columns through both
    /// `get_peer` and `list_peers`, and an upsert overwrites them. (libSQL mirror.)
    #[test]
    fn register_peer_full_roundtrips_pid_and_host() {
        let s = mem();
        s.register_peer_full(
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
        s.register_peer_full("p", "tmux", "%3", "", Some("/w"), None, "boxB", "", "", "")
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
        s2.register_peer_full("new", "tmux", "%2", "", None, Some(7), "h", "", "", "")
            .unwrap();
        let nrow = s2.get_peer("new").unwrap().unwrap();
        assert_eq!(nrow.pid, Some(7));
        assert_eq!(nrow.host, "h");
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
        let wr =
            ro.register_peer_full("intruder", "tmux", "%2", "", None, None, "boxA", "", "", "");
        assert!(
            wr.is_err(),
            "a write through a libsql read-only handle must error"
        );
        let send = ro.send("a", "b", None, "x");
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
            )
            .unwrap();
            rw.send("seed", "seed", None, "hi").unwrap();
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

        assert_trapped("send", ro.send("a", "b", None, "x").map(|_| ()));
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
            ro.register_peer_full("intruder", "tmux", "%2", "", None, None, "boxA", "", "", ""),
        );
        assert_trapped(
            "enqueue_intent",
            ro.enqueue_intent("to", "boxB", "from", None, "body", "")
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
            .register_peer_full("me", "tmux", "%1", "", None, None, "boxA", "", "", "")
            .unwrap();
        {
            let cfg = Config {
                db: Some(foreign_path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..Config::default()
            };
            let foreign = LibsqlStore::open(&cfg).unwrap();
            foreign
                .register_peer_full("them", "tmux", "%2", "", None, None, "boxA", "", "", "")
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
            .enqueue_intent("bob", "boxB", "alice", Some("hi"), "body1", "")
            .unwrap();
        s.enqueue_intent("carol", "", "alice", None, "for carol", "")
            .unwrap();
        let i3 = s
            .enqueue_intent("bob", "", "alice", None, "body3", "")
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

    /// The 2d `keys` table round-trips a registered pubkey through get/list and
    /// upserts on conflict (libsql mirror). Plain data; present regardless of the
    /// `sign` feature.
    #[test]
    fn keys_register_get_list_roundtrip_libsql() {
        let s = mem();
        assert!(s.get_key("alice").unwrap().is_none());
        s.register_key("alice", "aa11").unwrap();
        s.register_key("bob", "bb22").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));
        s.register_key("alice", "cc33").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("cc33"));
        let keys = s.list_keys().unwrap();
        assert_eq!(
            keys,
            vec![
                ("alice".to_string(), "cc33".to_string()),
                ("bob".to_string(), "bb22".to_string()),
            ]
        );
        assert!(s.register_key("", "00").is_err());
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
            a.enqueue_intent("bob", "", "alice", Some("hi"), "hello bob", "")
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
        let pulled = pull_from_store(&b, "bob", &allow, false).unwrap();
        assert_eq!(pulled.committed, 1);

        let (rows, _) = b.inbox("bob", false, false, 50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "alice");
        assert_eq!(rows[0].body, "hello bob");

        let again = pull_from_store(&b, "bob", &allow, false).unwrap();
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
}
