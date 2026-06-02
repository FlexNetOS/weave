//! Core data types shared across the store, injector, and MCP layers.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Recipient aliases that mean "deliver to every session". Single source of truth.
pub const BROADCAST: &[&str] = &["all", "*", "everyone", "broadcast"];

/// SQL fragment for the broadcast set, e.g. `('all','*','everyone','broadcast')`,
/// interpolated into the `recipient IN {bc}` delivery/unread/history filters.
///
/// This is a manually-maintained mirror of [`BROADCAST`]. It is `const` (so it can
/// be `format!`-interpolated cheaply) rather than generated at runtime, but the
/// `broadcast_sql_matches_broadcast` unit test asserts it stays byte-identical to
/// [`broadcast_sql`], which IS derived from [`BROADCAST`]. So a drift between the
/// Rust check and the SQL filter is caught at test time, not in production. The
/// values are compile-time constants (never user input), so embedding them as SQL
/// literals is safe.
pub const BROADCAST_SQL: &str = "('all','*','everyone','broadcast')";

/// Build the broadcast SQL fragment from [`BROADCAST`], single-quote-escaping each
/// alias. This is the source of truth that [`BROADCAST_SQL`] must equal; it exists
/// to back the `broadcast_sql_matches_broadcast` drift guard (hence test-only).
#[cfg(test)]
pub fn broadcast_sql() -> String {
    let inner = BROADCAST
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("({inner})")
}

pub fn is_broadcast(name: &str) -> bool {
    BROADCAST.contains(&name)
}

/// Current UNIX time in seconds. Stored as an integer so we need no date crate;
/// formatted for humans only at display time.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format a UNIX-seconds timestamp as `YYYY-MM-DDTHH:MM:SSZ` (UTC) without pulling
/// in a date crate. Uses Howard Hinnant's civil-from-days algorithm.
pub fn fmt_ts(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A stored message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub ts: i64,
    pub sender: String,
    pub recipient: String,
    pub subject: Option<String>,
    pub body: String,
    /// Id of the message this one replies to, if any. `None` for top-level
    /// messages. Additive + backward-compatible: pre-existing rows (and DBs
    /// created before the `in_reply_to` column migration) read back as `None`.
    /// `#[serde(default)]` keeps older JSON payloads (which omit the field)
    /// deserializable.
    #[serde(default)]
    pub in_reply_to: Option<i64>,
}

/// A session that has registered itself, with where (if anywhere) it can be
/// injected into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    /// "tmux" | "zellij" | "none"
    pub mux: String,
    /// tmux pane id (e.g. "%3") or zellij session name; empty for "none".
    pub target: String,
    pub cwd: Option<String>,
    pub last_seen: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-maintained `BROADCAST_SQL` literal must stay byte-identical to the
    /// fragment derived from `BROADCAST`. If anyone edits one without the other,
    /// the Rust `is_broadcast` check and the SQL `recipient IN {bc}` filters would
    /// disagree and corrupt delivery — this guard makes that drift a test failure.
    #[test]
    fn broadcast_sql_matches_broadcast() {
        assert_eq!(BROADCAST_SQL, broadcast_sql());
    }

    /// Every alias the Rust path treats as broadcast must appear in the SQL set.
    #[test]
    fn every_broadcast_alias_is_in_sql() {
        for alias in BROADCAST {
            assert!(is_broadcast(alias));
            assert!(
                broadcast_sql().contains(&format!("'{alias}'")),
                "alias {alias:?} missing from broadcast_sql()"
            );
        }
    }
}
