//! Persistent message + peer store.
//!
//! [`Store`] is the backend-agnostic interface. [`SqliteStore`] (rusqlite, bundled)
//! is the default; a feature-gated libSQL/Turso backend implements the same trait
//! for cross-machine sync (see `store_libsql.rs`). The on-disk SQLite format is
//! libSQL-compatible, so the file is portable between backends.

use crate::model::{is_broadcast, now, Message, Peer, BROADCAST_SQL};
use anyhow::Result;
use rusqlite::{params, Connection, Row};
use std::path::Path;
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
    fn unread_count(&self, me: &str) -> Result<i64>;
    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>>;
    fn sessions(&self) -> Result<Vec<SessionInfo>>;
    fn total_messages(&self) -> Result<i64>;
    fn clear_inbox(&self, me: &str) -> Result<usize>;
    fn clear_all(&self) -> Result<i64>;
    fn register_peer(&self, name: &str, mux: &str, target: &str, cwd: Option<&str>) -> Result<()>;
    fn get_peer(&self, name: &str) -> Result<Option<Peer>>;
    fn list_peers(&self) -> Result<Vec<Peer>>;
    /// Backend label for diagnostics.
    fn backend(&self) -> &'static str;
}

/// True if `last_seen` is within the online window relative to now.
pub fn is_online(last_seen: i64) -> bool {
    now().saturating_sub(last_seen) <= ONLINE_TTL_SECS
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    sender    TEXT NOT NULL,
    recipient TEXT NOT NULL,
    subject   TEXT,
    body      TEXT NOT NULL
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

pub struct SqliteStore {
    conn: Connection,
}

fn row_to_message(r: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get("id")?,
        ts: r.get("ts")?,
        sender: r.get("sender")?,
        recipient: r.get("recipient")?,
        subject: r.get("subject")?,
        body: r.get("body")?,
    })
}

fn row_to_peer(r: &Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        name: r.get(0)?,
        mux: r.get(1)?,
        target: r.get(2)?,
        cwd: r.get(3)?,
        last_seen: r.get(4)?,
    })
}

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
        Ok(Self { conn })
    }
}

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
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows: Vec<Message> = stmt
            .query_map(params![me, limit], row_to_message)?
            .collect::<rusqlite::Result<_>>()?;
        rows.reverse();

        if mark_read && !rows.is_empty() {
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
        }
        let remaining = self.unread_count(me)?;
        Ok((rows, remaining))
    }

    fn unread_count(&self, me: &str) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM messages m
             WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
               AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
            bc = BROADCAST_SQL
        );
        Ok(self.conn.query_row(&sql, params![me], |r| r.get(0))?)
    }

    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>> {
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
            let mut stmt = self.conn.prepare("SELECT DISTINCT recipient FROM messages")?;
            for n in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let n = n?;
                if !is_broadcast(&n) {
                    names.push(n);
                }
            }
        }
        names.sort();
        names.dedup();

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
            let mut ins = tx
                .prepare("INSERT OR IGNORE INTO reads (message_id, reader, ts) VALUES (?1,?2,?3)")?;
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
}

#[cfg(test)]
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
}
