//! Persistent message + peer store.
//!
//! [`Store`] is the backend-agnostic interface. [`SqliteStore`] (rusqlite, bundled)
//! is the default; a feature-gated libSQL/Turso backend implements the same trait
//! for cross-machine sync (see `store_libsql.rs`). The on-disk SQLite format is
//! libSQL-compatible, so the file is portable between backends.

use crate::model::{now, Message, Peer};
use anyhow::Result;

#[cfg(feature = "sqlite")]
use crate::model::{is_broadcast, BROADCAST_SQL};
#[cfg(feature = "sqlite")]
use rusqlite::{params, Connection, Row, Transaction, TransactionBehavior};
#[cfg(feature = "sqlite")]
use std::path::Path;
#[cfg(feature = "sqlite")]
use std::time::Duration;

/// A peer is considered "online" if its last heartbeat is within this window.
pub const ONLINE_TTL_SECS: i64 = 900;

/// (name, unread, last_activity_ts)
pub type SessionInfo = (String, i64, i64);

/// Backend-agnostic store interface. Object-safe so the app can hold a
/// `Box<dyn Store>` and pick the backend at runtime.
pub trait Store: Send {
    fn send(&self, sender: &str, recipient: &str, subject: Option<&str>, body: &str)
        -> Result<i64>;
    fn inbox(
        &self,
        me: &str,
        include_read: bool,
        mark_read: bool,
        limit: i64,
    ) -> Result<(Vec<Message>, i64)>;
    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>>;
    fn sessions(&self) -> Result<Vec<SessionInfo>>;
    fn total_messages(&self) -> Result<i64>;
    fn clear_inbox(&self, me: &str) -> Result<usize>;
    fn clear_all(&self) -> Result<i64>;
    /// Delete messages (and their read-markers) older than `older_than_secs`.
    /// Returns how many messages were removed. Retention / disk-bound guard.
    fn gc(&self, older_than_secs: i64) -> Result<i64>;
    fn register_peer(&self, name: &str, mux: &str, target: &str, cwd: Option<&str>) -> Result<()>;
    fn get_peer(&self, name: &str) -> Result<Option<Peer>>;
    fn list_peers(&self) -> Result<Vec<Peer>>;
    /// Backend label for diagnostics.
    fn backend(&self) -> &'static str;

    /// Reply to an existing message. The parent (`in_reply_to`) is looked up so
    /// the reply is automatically addressed back to whoever wrote it (i.e. the
    /// other party of the parent, from `sender`'s perspective): if `sender`
    /// authored the parent, the reply goes to the parent's recipient; otherwise
    /// it goes to the parent's sender. The parent's `subject` is inherited
    /// (prefixed once with `Re: ` if not already). Returns the new message id.
    ///
    /// Default implementation in terms of the existing primitives so backends
    /// only override when they want a tighter (single-transaction) version.
    fn reply(&self, sender: &str, in_reply_to: i64, body: &str) -> Result<i64> {
        let (recipient, subject) = self.reply_target(sender, in_reply_to)?;
        let id = self.send(sender, &recipient, subject.as_deref(), body)?;
        self.set_in_reply_to(id, in_reply_to)?;
        Ok(id)
    }

    /// Fetch a thread rooted at `root_id`: the root message itself plus every
    /// message whose `in_reply_to` (transitively) leads back to it, ordered
    /// oldest-first and capped at `limit`. The threading is resolved with a
    /// recursive CTE so deep chains do not incur an N+1 of round-trips.
    fn thread(&self, root_id: i64, limit: i64) -> Result<Vec<Message>>;

    /// Read receipts for a message: `(reader, ts)` pairs from the `reads` table,
    /// oldest-first. Lets a sender see who has seen a given message and when.
    fn receipts(&self, message_id: i64) -> Result<Vec<(String, i64)>>;

    /// Refresh a peer's `last_seen` to now WITHOUT touching its mux/target/cwd.
    /// A no-op if the peer does not exist (heartbeat-only; registration is
    /// `register_peer`'s job).
    fn touch_peer(&self, name: &str) -> Result<()>;

    /// Resolve the `(recipient, subject)` a reply to `in_reply_to` should carry,
    /// from `sender`'s perspective. Internal seam for the default `reply` impl;
    /// backends implement it cheaply against their own connection.
    fn reply_target(&self, sender: &str, in_reply_to: i64) -> Result<(String, Option<String>)>;

    /// Stamp an already-inserted message's `in_reply_to` column. Internal seam
    /// for the default `reply` impl.
    fn set_in_reply_to(&self, message_id: i64, in_reply_to: i64) -> Result<()>;
}

/// True if `last_seen` is within the online window relative to now.
pub fn is_online(last_seen: i64) -> bool {
    now().saturating_sub(last_seen) <= ONLINE_TTL_SECS
}

/// Hard upper bound on a query `LIMIT`. A negative limit means *unbounded* in
/// SQLite, so untrusted limits (from MCP/CLI) are clamped here to prevent an
/// accidental or hostile unbounded fetch.
pub const MAX_LIMIT: i64 = 10_000;

/// Hard upper bound on a stored message body (bytes). Peer-supplied bodies are
/// untrusted; unbounded ones are a disk + token/RAM DoS once re-rendered into
/// another agent's context. Enforced at the store layer so CLI/MCP/hook are all
/// covered.
pub const MAX_BODY: usize = 65_536;

/// Reject an over-length body before it is stored (shared by both backends).
pub fn check_body(body: &str) -> Result<()> {
    if body.len() > MAX_BODY {
        anyhow::bail!(
            "message body is too long ({} bytes; max {MAX_BODY}).",
            body.len()
        );
    }
    Ok(())
}

/// Clamp an untrusted limit into `[0, MAX_LIMIT]`, mapping negatives to the cap
/// (callers that want "a lot" pass a big/negative number; they get the cap, not
/// an unbounded scan).
pub fn clamp_limit(limit: i64) -> i64 {
    if limit < 0 {
        MAX_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

/// Hard ceiling on how many distinct sessions `sessions()` will expand. Each
/// session triggers per-name unread + last-activity sub-queries (an inherent
/// N+1), so an unbounded participant set would let a busy/hostile DB turn one
/// `sessions` call into thousands of round-trips. Names beyond this ceiling
/// (already sorted) are dropped from the result. Generous for any real mesh.
pub const MAX_SESSIONS: usize = 1_000;

/// Derive the subject a reply should carry from its parent's subject: inherit
/// it, prefixing `Re: ` exactly once (case-insensitive, so we never stack
/// `Re: Re: ...`). A parent with no subject yields `None`.
pub fn reply_subject(parent_subject: Option<&str>) -> Option<String> {
    parent_subject.map(|s| {
        let trimmed = s.trim_start();
        if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("re:") {
            s.to_string()
        } else {
            format!("Re: {s}")
        }
    })
}

#[cfg(feature = "sqlite")]
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    sender      TEXT NOT NULL,
    recipient   TEXT NOT NULL,
    subject     TEXT,
    body        TEXT NOT NULL,
    in_reply_to INTEGER
);
CREATE TABLE IF NOT EXISTS reads (
    message_id INTEGER NOT NULL,
    reader     TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (message_id, reader)
);
CREATE TABLE IF NOT EXISTS peers (
    name      TEXT PRIMARY KEY,
    mux       TEXT NOT NULL,
    target    TEXT NOT NULL,
    cwd       TEXT,
    last_seen INTEGER NOT NULL
);
";

#[cfg(feature = "sqlite")]
pub struct SqliteStore {
    conn: Connection,
}

#[cfg(feature = "sqlite")]
fn row_to_message(r: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get("id")?,
        ts: r.get("ts")?,
        sender: r.get("sender")?,
        recipient: r.get("recipient")?,
        subject: r.get("subject")?,
        body: r.get("body")?,
        // Read by name so projections that include the column populate it; the
        // migration guarantees the column exists, so this never errors on a
        // `SELECT *`. Mappers over projections that omit the column (e.g. the
        // explicit `SELECT id, ts, ...` thread CTE adds it deliberately) supply
        // it themselves rather than calling this helper.
        in_reply_to: r.get("in_reply_to").unwrap_or(None),
    })
}

/// Count unread messages for `me` against an arbitrary connection (the live
/// connection or an open transaction), so the count can share a transaction with
/// the inbox read+mark for a consistent snapshot.
#[cfg(feature = "sqlite")]
fn unread_count_conn(conn: &Connection, me: &str) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
        bc = BROADCAST_SQL
    );
    Ok(conn.query_row(&sql, params![me], |r| r.get(0))?)
}

#[cfg(feature = "sqlite")]
fn row_to_peer(r: &Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        name: r.get(0)?,
        mux: r.get(1)?,
        target: r.get(2)?,
        cwd: r.get(3)?,
        last_seen: r.get(4)?,
    })
}

/// True if table `table` already has a column named `column`. Uses
/// `pragma_table_info` so a migration can be made idempotent (an `ALTER TABLE
/// ADD COLUMN` would otherwise error if the column is already present).
#[cfg(feature = "sqlite")]
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    let exists = stmt.exists(params![table, column])?;
    Ok(exists)
}

/// Apply additive, backward-compatible migrations to an already-open DB so that
/// databases created by an older weave gain new columns in place. Each step is
/// guarded by an existence check, so running this repeatedly is a no-op.
#[cfg(feature = "sqlite")]
fn migrate(conn: &Connection) -> Result<()> {
    // messages.in_reply_to — present on fresh DBs via SCHEMA, added here for
    // DBs created before threading existed. SQLite `ADD COLUMN` is O(1) and the
    // new column defaults to NULL for every existing row (== top-level message).
    if !column_exists(conn, "messages", "in_reply_to")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN in_reply_to INTEGER;")?;
    }
    Ok(())
}

/// Best-effort tighten the DB file to owner-only (0600) on unix so message
/// bodies are not world-readable. Failure is non-fatal: on a filesystem that
/// does not honour unix permissions (or if we do not own the file) weave still
/// works, it is just not hardened. No-op on non-unix targets.
#[cfg(all(feature = "sqlite", unix))]
fn harden_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(all(feature = "sqlite", not(unix)))]
fn harden_permissions(_path: &Path) {}

#[cfg(feature = "sqlite")]
impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        // journal_mode returns a row, so query rather than execute.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        // Restrict the on-disk DB (which holds message bodies) to the owner.
        // Done after the file is guaranteed to exist (post-open) and is
        // best-effort so it never breaks startup on odd filesystems.
        harden_permissions(path);
        Ok(Self { conn })
    }

    /// Unread messages for `me` (inherent helper; used by `sessions`).
    fn unread_count(&self, me: &str) -> Result<i64> {
        unread_count_conn(&self.conn, me)
    }
}

#[cfg(feature = "sqlite")]
impl Store for SqliteStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn send(
        &self,
        sender: &str,
        recipient: &str,
        subject: Option<&str>,
        body: &str,
    ) -> Result<i64> {
        check_body(body)?;
        self.conn.execute(
            "INSERT INTO messages (ts, sender, recipient, subject, body) VALUES (?1,?2,?3,?4,?5)",
            params![now(), sender, recipient, subject, body],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn inbox(
        &self,
        me: &str,
        include_read: bool,
        mark_read: bool,
        limit: i64,
    ) -> Result<(Vec<Message>, i64)> {
        let limit = clamp_limit(limit);
        let sql = if include_read {
            format!(
                "SELECT * FROM messages
                 WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                 ORDER BY id DESC LIMIT ?2",
                bc = BROADCAST_SQL
            )
        } else {
            format!(
                "SELECT m.* FROM messages m
                 WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
                   AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)
                 ORDER BY m.id DESC LIMIT ?2",
                bc = BROADCAST_SQL
            )
        };

        // Run the SELECT, the read-marking, and the remaining count inside ONE
        // IMMEDIATE transaction so the returned rows, the marks, and `remaining`
        // are a single consistent snapshot — a concurrent writer cannot slip a
        // message in between the read and the count.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let mut rows: Vec<Message> = {
            let mut stmt = tx.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, limit], row_to_message)?
                .collect::<rusqlite::Result<_>>()?;
            v
        };
        rows.reverse();

        if mark_read && !rows.is_empty() {
            let ts = now();
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO reads (message_id, reader, ts) VALUES (?1,?2,?3)",
            )?;
            for m in &rows {
                ins.execute(params![m.id, me, ts])?;
            }
        }

        let remaining = unread_count_conn(&tx, me)?;
        tx.commit()?;
        Ok((rows, remaining))
    }

    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        let mut rows: Vec<Message> = if let Some(p) = peer {
            let sql = format!(
                "SELECT * FROM messages
                 WHERE (sender = ?1 AND (recipient = ?2 OR recipient IN {bc}))
                    OR (sender = ?2 AND (recipient = ?1 OR recipient IN {bc}))
                 ORDER BY id DESC LIMIT ?3",
                bc = BROADCAST_SQL
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, p, limit], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        } else {
            let sql = format!(
                "SELECT * FROM messages
                 WHERE sender = ?1 OR recipient = ?1 OR recipient IN {bc}
                 ORDER BY id DESC LIMIT ?2",
                bc = BROADCAST_SQL
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, limit], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        rows.reverse();
        Ok(rows)
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut names: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn.prepare("SELECT DISTINCT sender FROM messages")?;
            for n in stmt.query_map([], |r| r.get::<_, String>(0))? {
                names.push(n?);
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT recipient FROM messages")?;
            for n in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let n = n?;
                if !is_broadcast(&n) {
                    names.push(n);
                }
            }
        }
        names.sort();
        names.dedup();
        // Ceiling the per-name N+1 (unread + last-activity sub-queries). Names
        // are already sorted, so this deterministically keeps the first
        // `MAX_SESSIONS`.
        names.truncate(MAX_SESSIONS);

        let mut out = Vec::new();
        for n in names {
            let unread = self.unread_count(&n)?;
            let last: i64 = self
                .conn
                .query_row(
                    "SELECT COALESCE(MAX(ts),0) FROM messages WHERE sender=?1 OR recipient=?1",
                    params![n],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            out.push((n, unread, last));
        }
        Ok(out)
    }

    fn total_messages(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }

    fn clear_inbox(&self, me: &str) -> Result<usize> {
        let (rows, _) = self.inbox(me, false, false, i64::MAX)?;
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO reads (message_id, reader, ts) VALUES (?1,?2,?3)",
            )?;
            for m in &rows {
                ins.execute(params![m.id, me, ts])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    fn clear_all(&self) -> Result<i64> {
        let n = self.total_messages()?;
        self.conn
            .execute_batch("DELETE FROM messages; DELETE FROM reads;")?;
        Ok(n)
    }

    fn gc(&self, older_than_secs: i64) -> Result<i64> {
        let cutoff = now().saturating_sub(older_than_secs.max(0));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let n: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE ts < ?1",
            params![cutoff],
            |r| r.get(0),
        )?;
        tx.execute(
            "DELETE FROM reads WHERE message_id IN (SELECT id FROM messages WHERE ts < ?1)",
            params![cutoff],
        )?;
        tx.execute("DELETE FROM messages WHERE ts < ?1", params![cutoff])?;
        tx.commit()?;
        Ok(n)
    }

    fn register_peer(&self, name: &str, mux: &str, target: &str, cwd: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO peers (name, mux, target, cwd, last_seen) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(name) DO UPDATE SET mux=?2, target=?3, cwd=?4, last_seen=?5",
            params![name, mux, target, cwd, now()],
        )?;
        Ok(())
    }

    fn get_peer(&self, name: &str) -> Result<Option<Peer>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, mux, target, cwd, last_seen FROM peers WHERE name=?1")?;
        let mut it = stmt.query_map(params![name], row_to_peer)?;
        match it.next() {
            Some(p) => Ok(Some(p?)),
            None => Ok(None),
        }
    }

    fn list_peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, mux, target, cwd, last_seen FROM peers ORDER BY name")?;
        let rows = stmt
            .query_map([], row_to_peer)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn reply_target(&self, sender: &str, in_reply_to: i64) -> Result<(String, Option<String>)> {
        // Look up the parent's sender/recipient/subject, then address the reply
        // to the *other* party from `sender`'s perspective.
        let (psender, precipient, psubject): (String, String, Option<String>) =
            self.conn.query_row(
                "SELECT sender, recipient, subject FROM messages WHERE id = ?1",
                params![in_reply_to],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let recipient = if psender == sender {
            precipient
        } else {
            psender
        };
        Ok((recipient, reply_subject(psubject.as_deref())))
    }

    fn set_in_reply_to(&self, message_id: i64, in_reply_to: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET in_reply_to = ?1 WHERE id = ?2",
            params![in_reply_to, message_id],
        )?;
        Ok(())
    }

    fn reply(&self, sender: &str, in_reply_to: i64, body: &str) -> Result<i64> {
        // One transaction so the parent lookup, the insert, and the
        // in_reply_to stamp are atomic (the parent cannot vanish mid-reply).
        let (recipient, subject) = self.reply_target(sender, in_reply_to)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![now(), sender, recipient, subject, body, in_reply_to],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn thread(&self, root_id: i64, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        // Recursive CTE walks root → every (transitive) reply in one query,
        // avoiding an N+1 of per-level lookups. Ordered oldest-first (by id) so
        // the conversation reads top-to-bottom.
        let sql = "
            WITH RECURSIVE t(id) AS (
                SELECT id FROM messages WHERE id = ?1
                UNION
                SELECT m.id FROM messages m JOIN t ON m.in_reply_to = t.id
            )
            SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to
            FROM messages m JOIN t ON m.id = t.id
            ORDER BY m.id ASC LIMIT ?2";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![root_id, limit], |r| {
                Ok(Message {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    sender: r.get(2)?,
                    recipient: r.get(3)?,
                    subject: r.get(4)?,
                    body: r.get(5)?,
                    in_reply_to: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn receipts(&self, message_id: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT reader, ts FROM reads WHERE message_id = ?1 ORDER BY ts ASC, reader ASC",
        )?;
        let rows = stmt
            .query_map(params![message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn touch_peer(&self, name: &str) -> Result<()> {
        // Heartbeat only: refresh last_seen, never create or alter mux/target.
        self.conn.execute(
            "UPDATE peers SET last_seen = ?1 WHERE name = ?2",
            params![now(), name],
        )?;
        Ok(())
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    fn mem() -> SqliteStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weave-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        SqliteStore::open(&dir.join("t.db")).unwrap()
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
        s.register_peer("envctl", "zellij", "envctl", Some("/home/x/envctl"))
            .unwrap();
        s.register_peer("envctl", "tmux", "%4", Some("/home/x/envctl"))
            .unwrap();
        let p = s.get_peer("envctl").unwrap().unwrap();
        assert_eq!(p.mux, "tmux");
        assert_eq!(p.target, "%4");
        assert!(is_online(p.last_seen));
        assert!(!is_online(p.last_seen - ONLINE_TTL_SECS - 1));
        assert_eq!(s.list_peers().unwrap().len(), 1);
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
    fn clamp_limit_bounds() {
        assert_eq!(
            clamp_limit(-1),
            MAX_LIMIT,
            "negative maps to the cap, not unbounded"
        );
        assert_eq!(clamp_limit(i64::MIN), MAX_LIMIT);
        assert_eq!(clamp_limit(0), 0);
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(i64::MAX), MAX_LIMIT);
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
    }

    #[test]
    fn gc_deletes_old_keeps_new() {
        let s = mem();
        let id_old = s.send("a", "b", None, "old").unwrap();
        // Backdate the first message well past the threshold.
        s.conn
            .execute(
                "UPDATE messages SET ts = ts - 100000 WHERE id = ?1",
                params![id_old],
            )
            .unwrap();
        s.send("a", "b", None, "new").unwrap();
        let deleted = s.gc(3600).unwrap(); // older than 1h
        assert_eq!(deleted, 1);
        assert_eq!(s.total_messages().unwrap(), 1);
        let (rows, _) = s.inbox("b", true, false, 50).unwrap();
        assert_eq!(rows[0].body, "new");
    }

    #[test]
    fn reply_addresses_back_and_links() {
        let s = mem();
        // a -> b "hi". b replies; the reply must go back to a, carry "Re: hi",
        // and link to the parent via in_reply_to.
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
        // The unrelated top-level message is not pulled into the thread.
        assert!(thread.iter().all(|m| m.body != "unrelated"));
    }

    #[test]
    fn receipts_reports_readers() {
        let s = mem();
        let id = s.send("a", "all", None, "ping").unwrap();
        assert!(s.receipts(id).unwrap().is_empty(), "nobody has read yet");

        // Two recipients read the broadcast (mark_read), creating receipts.
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
    fn touch_peer_refreshes_without_clobbering() {
        let s = mem();
        s.register_peer("envctl", "tmux", "%7", Some("/w")).unwrap();
        // Backdate last_seen, then touch and confirm only last_seen advanced.
        s.conn
            .execute(
                "UPDATE peers SET last_seen = last_seen - 100000 WHERE name = 'envctl'",
                [],
            )
            .unwrap();
        let before = s.get_peer("envctl").unwrap().unwrap();
        s.touch_peer("envctl").unwrap();
        let after = s.get_peer("envctl").unwrap().unwrap();
        assert!(after.last_seen > before.last_seen);
        assert_eq!(after.mux, "tmux");
        assert_eq!(after.target, "%7");
        assert_eq!(after.cwd.as_deref(), Some("/w"));

        // Touching an unknown peer is a silent no-op (no row created).
        s.touch_peer("ghost").unwrap();
        assert!(s.get_peer("ghost").unwrap().is_none());
    }

    #[test]
    fn reply_subject_prefix_is_idempotent() {
        assert_eq!(reply_subject(None), None);
        assert_eq!(reply_subject(Some("hi")).as_deref(), Some("Re: hi"));
        // Already-prefixed subjects are not re-prefixed (case-insensitive).
        assert_eq!(reply_subject(Some("Re: hi")).as_deref(), Some("Re: hi"));
        assert_eq!(reply_subject(Some("RE: hi")).as_deref(), Some("RE: hi"));
        assert_eq!(reply_subject(Some("re: hi")).as_deref(), Some("re: hi"));
    }

    #[test]
    fn db_file_is_owner_only() {
        // The hardening step must leave the DB file at mode 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("weave-perms-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("perms.db");
            let _s = SqliteStore::open(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "db file should be owner-only");
        }
    }
}
