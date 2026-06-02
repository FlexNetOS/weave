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
use crate::store::{
    check_body, check_ident, clamp_limit, reply_subject, SessionInfo, Store, MAX_SESSIONS,
};
use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database, Value};
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
        name      TEXT PRIMARY KEY,
        mux       TEXT NOT NULL,
        target    TEXT NOT NULL,
        socket    TEXT NOT NULL DEFAULT '',
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

/// Column order: name, mux, target, socket, cwd, last_seen.
fn row_to_peer(r: &libsql::Row) -> Result<Peer> {
    Ok(Peer {
        name: r.get::<String>(0)?,
        mux: r.get::<String>(1)?,
        target: r.get::<String>(2)?,
        socket: r.get::<String>(3)?,
        cwd: r.get::<Option<String>>(4)?,
        last_seen: r.get::<i64>(5)?,
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

    fn register_peer(
        &self,
        name: &str,
        mux: &str,
        target: &str,
        socket: &str,
        cwd: Option<&str>,
    ) -> Result<()> {
        check_ident("peer name", name)?;
        self.rt.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO peers (name, mux, target, socket, cwd, last_seen) VALUES (?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(name) DO UPDATE SET mux=?2, target=?3, socket=?4, cwd=?5, last_seen=?6",
                    params(vec![
                        name.into(),
                        mux.into(),
                        target.into(),
                        socket.into(),
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
                    "SELECT name, mux, target, socket, cwd, last_seen FROM peers WHERE name=?1",
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
                    "SELECT name, mux, target, socket, cwd, last_seen FROM peers ORDER BY name",
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
    use crate::store::{clamp_limit, is_online, MAX_IDENT, MAX_LIMIT, ONLINE_TTL_SECS};

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
}
