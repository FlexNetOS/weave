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

use crate::config::Config;
use crate::model::{is_broadcast, now, Message, Peer, BROADCAST_SQL};
use crate::store::{SessionInfo, Store};
use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database, Value};
use tokio::runtime::Runtime;

/// Same schema as `SqliteStore`. Executed statement-by-statement because the
/// libsql remote/HTTP path runs one statement per round-trip; `execute_batch`
/// works for local but splitting keeps both backends identical.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS messages (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        ts        INTEGER NOT NULL,
        sender    TEXT NOT NULL,
        recipient TEXT NOT NULL,
        subject   TEXT,
        body      TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS reads (
        message_id INTEGER NOT NULL,
        reader     TEXT NOT NULL,
        ts         INTEGER NOT NULL,
        PRIMARY KEY (message_id, reader)
    )",
    "CREATE TABLE IF NOT EXISTS peers (
        name      TEXT PRIMARY KEY,
        mux       TEXT NOT NULL,
        target    TEXT NOT NULL,
        cwd       TEXT,
        last_seen INTEGER NOT NULL
    )",
];

pub struct LibsqlStore {
    rt: Runtime,
    conn: Connection,
    // Keep the database alive for as long as the connection is used.
    _db: Database,
}

/// Convert a libsql row column into our owned `Message`. Column order matches
/// `SELECT *` / the explicit projections used below: id, ts, sender, recipient,
/// subject, body.
fn row_to_message(r: &libsql::Row) -> Result<Message> {
    Ok(Message {
        id: r.get::<i64>(0)?,
        ts: r.get::<i64>(1)?,
        sender: r.get::<String>(2)?,
        recipient: r.get::<String>(3)?,
        subject: r.get::<Option<String>>(4)?,
        body: r.get::<String>(5)?,
    })
}

/// Column order: name, mux, target, cwd, last_seen.
fn row_to_peer(r: &libsql::Row) -> Result<Peer> {
    Ok(Peer {
        name: r.get::<String>(0)?,
        mux: r.get::<String>(1)?,
        target: r.get::<String>(2)?,
        cwd: r.get::<Option<String>>(3)?,
        last_seen: r.get::<i64>(4)?,
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
            Ok::<_, anyhow::Error>((db, conn))
        })?;

        Ok(Self { rt, conn, _db: db })
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
        self.rt.block_on(async {
            let sql = if include_read {
                format!(
                    "SELECT id, ts, sender, recipient, subject, body FROM messages
                     WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                     ORDER BY id DESC LIMIT ?2",
                    bc = BROADCAST_SQL
                )
            } else {
                format!(
                    "SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body FROM messages m
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
        self.rt.block_on(async {
            let mut rows: Vec<Message> = Vec::new();
            if let Some(p) = peer {
                let sql = format!(
                    "SELECT id, ts, sender, recipient, subject, body FROM messages
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
                    "SELECT id, ts, sender, recipient, subject, body FROM messages
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

    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        self.rt.block_on(async {
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
        self.rt.block_on(async {
            let n = self.total_messages_async().await?;
            self.conn.execute("DELETE FROM messages", ()).await?;
            self.conn.execute("DELETE FROM reads", ()).await?;
            Ok(n)
        })
    }

    fn register_peer(&self, name: &str, mux: &str, target: &str, cwd: Option<&str>) -> Result<()> {
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO peers (name, mux, target, cwd, last_seen) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(name) DO UPDATE SET mux=?2, target=?3, cwd=?4, last_seen=?5",
                    params(vec![
                        name.into(),
                        mux.into(),
                        target.into(),
                        cwd.map(|s| s.to_string()).into(),
                        now().into(),
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
                    "SELECT name, mux, target, cwd, last_seen FROM peers WHERE name=?1",
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
        self.rt.block_on(async {
            let mut it = self
                .conn
                .query(
                    "SELECT name, mux, target, cwd, last_seen FROM peers ORDER BY name",
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
}
