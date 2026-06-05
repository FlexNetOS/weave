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

/// A cross-store delivery **intent** (Tier-2). An intent is an owner-written row
/// in the sender's own `outbox` table describing a message addressed to a peer
/// living in a *different* store. The sender never writes the recipient's store;
/// the recipient's own process pulls the intent (read-only) and commits it into
/// its own inbox via the normal `Store::send` path (owner-only-writes).
///
/// Pure data (no I/O), shared by both store backends and the `main`/`mcp`
/// consumers — the `Message`/`Peer` precedent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    /// The sender's local, per-store monotonic intent id (`outbox.id`,
    /// `AUTOINCREMENT`). The receiver dedups on `(source, this id)` via its own
    /// per-source `pull_cursor` high-water mark.
    pub id: i64,
    /// The sender's created time (advisory display only; the receiver re-stamps
    /// with its own `now()` when it commits, anchoring ordering locally).
    pub ts: i64,
    /// Recipient identity in the receiver's store.
    pub to: String,
    /// Optional host hint disambiguating the same name across machines
    /// (advisory). Empty when unspecified.
    #[serde(default)]
    pub to_host: String,
    /// Sender identity. Attributed to the source store on the receiver side
    /// (origin attribution); `from` is advisory within a store until signed
    /// identity (2d) makes it unforgeable.
    pub from: String,
    pub subject: Option<String>,
    pub body: String,
    /// Reserved signature over the canonical message bytes. Empty in 2a/2b;
    /// populated only by the optional `sign` feature (2d). Reserving the field
    /// now means 2d adds no further `outbox` migration. `#[serde(default)]` keeps
    /// older JSON payloads (which omit the field) deserializable.
    #[serde(default)]
    pub sig: String,
}

/// Hard upper bound (in chars) on a tracked-ask correlation id. The id is always
/// server-minted (`ask_<rowid>_<nonce>`), so it can never legitimately be long;
/// the cap exists to reject a hostile/oversized user-supplied REFERENCE id on
/// `answer`/`ack`/`get` before it is bound into a query, the `MAX_IDENT`-analog
/// the brief requires. 64 is far more than any minted id needs.
pub const MAX_ASK_ID_LEN: usize = 64;

/// Validate a tracked-ask correlation id. Accepts only the minted shape:
/// non-empty, `<= MAX_ASK_ID_LEN` chars, ASCII `[A-Za-z0-9_]` only. This is the
/// `inject::id_valid`-analog for correlation ids — it guards every store/MCP/CLI
/// path that takes a user-supplied id so a metachar-bearing or oversized value is
/// rejected before any DB bind (defense-in-depth even though all SQL is already
/// parameterized; never reaches a shell).
pub fn ask_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ASK_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Process-local monotonic counter feeding [`new_ask_id`]'s nonce. Combined with
/// the asks rowid and `now()` it makes a minted id unique within a process run
/// WITHOUT a `rand` crate (weave is deliberately dependency-light and has no date
/// crate either — the same discipline). The rowid alone already guarantees DB
/// uniqueness (it is the PK); the counter+ts only widen the opaque tail so the id
/// is not trivially guessable/sequential-looking.
static ASK_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint an opaque correlation id for a freshly-inserted asks row: `ask_<rowid>_<n>`
/// where `<n>` is a process-local nonce derived from `now()` + a monotonic counter
/// (NO `rand` dependency). `rowid` is the asks PK, so the id is unique in the DB;
/// the nonce only widens the opaque tail. The result always satisfies
/// [`ask_id_valid`] (digits + `_` only). Deterministic-test-friendly: callers that
/// need a fixed id in a test can assert the `ask_<rowid>_` prefix rather than the
/// full string.
pub fn new_ask_id(rowid: i64) -> String {
    let n = ASK_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Mix the wall clock and the counter into a single non-negative tail. Using
    // `as u64` keeps it digits-only so `ask_id_valid` always accepts the output.
    let nonce = (now() as u64).wrapping_mul(1_000_003).wrapping_add(n);
    format!("ask_{rowid}_{nonce}")
}

/// Lifecycle state of a tracked ask. The canonical P1 vocabulary is
/// `open → answered → acked`; it is stored as TEXT (see the `asks` table) and
/// validated through this enum so a future epic can ADD variants (e.g. the richer
/// SEP-1686 set) with no schema migration. The machine is **monotonic**: it never
/// moves backward (see [`AskState::can_transition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskState {
    /// Opened, awaiting an answer.
    Open,
    /// The askee replied; the answer message id is recorded on the asks row.
    Answered,
    /// The thread is closed (acked); `closed_ts` is stamped.
    Acked,
}

impl AskState {
    /// Canonical lowercase label stored in the `asks.state` TEXT column. The only
    /// inlined SQL "literals" for state are derived from this (compile-time
    /// constants, never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            AskState::Open => "open",
            AskState::Answered => "answered",
            AskState::Acked => "acked",
        }
    }

    /// Parse a stored state string back into the enum. An unknown value is a hard
    /// error at the store mapper (never a panic, never silently coerced) so a
    /// corrupt/foreign row surfaces loudly rather than mis-driving the machine.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "open" => Ok(AskState::Open),
            "answered" => Ok(AskState::Answered),
            "acked" => Ok(AskState::Acked),
            other => Err(format!("unknown ask state '{other}'")),
        }
    }

    /// The monotonic lifecycle machine: the ONLY legal edges are
    /// `Open→Answered`, `Open→Acked`, and `Answered→Acked`. Everything else —
    /// any `→Open`, any `Acked→*`, and every self-edge — is rejected, so the
    /// lifecycle can never move backward or repeat (this is the pure invariant the
    /// proptest targets). The store consults this before every state UPDATE.
    pub fn can_transition(self, to: AskState) -> bool {
        matches!(
            (self, to),
            (AskState::Open, AskState::Answered)
                | (AskState::Open, AskState::Acked)
                | (AskState::Answered, AskState::Acked)
        )
    }
}

/// Which side of an ask a `list_asks` query filters on. `Asker` = asks I opened;
/// `Askee` = asks addressed to me; `Any` = either. Pure data (no I/O), shared by
/// the store + the mcp/main consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskRole {
    Asker,
    Askee,
    Any,
}

impl AskRole {
    /// Parse the CLI/MCP `role` string; an empty/unknown value defaults to `Any`
    /// (the safest superset — never narrows a listing unexpectedly).
    pub fn parse(s: &str) -> AskRole {
        match s.trim().to_ascii_lowercase().as_str() {
            "asker" => AskRole::Asker,
            "askee" => AskRole::Askee,
            _ => AskRole::Any,
        }
    }
}

/// A tracked request/response thread (P1 ask/answer/ack). The question and answer
/// TEXT live in the `messages` table (threaded via `in_reply_to`); this row holds
/// only the correlation id + the mutable lifecycle, pointing at those messages by
/// id. Mirrors the `reads` side-table pattern (mutable side-state keyed to a
/// message, same DB, both backends).
///
/// `Serialize/Deserialize` with `#[serde(default)]` on the nullable/added fields
/// (the `Message`/`Intent`/`Peer` precedent) so older JSON payloads stay
/// deserializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ask {
    /// Opaque correlation id (`ask_<rowid>_<nonce>`); the PK.
    pub id: String,
    /// `messages` row carrying the question text.
    pub question_msg_id: i64,
    /// `messages` row carrying the answer text; `None` until answered.
    #[serde(default)]
    pub answer_msg_id: Option<i64>,
    pub asker: String,
    pub askee: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub state: AskState,
    /// Prior ask id this one chains/closes (`None` for a root ask).
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Optional closing note from `ack` (`None` otherwise).
    #[serde(default)]
    pub close_note: Option<String>,
    pub opened_ts: i64,
    pub updated_ts: i64,
    /// Set when the thread reaches `acked`.
    #[serde(default)]
    pub closed_ts: Option<i64>,
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
    /// Multiplexer control socket (e.g. kitty's `KITTY_LISTEN_ON`); empty when
    /// unknown or not applicable. Additive + backward-compatible: pre-existing
    /// rows (and DBs created before the `socket` column migration) read back as
    /// `""`. `#[serde(default)]` keeps older JSON payloads (which omit the
    /// field) deserializable.
    #[serde(default)]
    pub socket: String,
    pub cwd: Option<String>,
    pub last_seen: i64,
    /// PID of the process that registered this peer, if known. Used (together
    /// with [`Peer::host`]) for real process-liveness on the local host. `None`
    /// ⇒ unknown ⇒ presence falls back to the recency (TTL) guess. Additive +
    /// backward-compatible: pre-existing rows (and DBs created before the `pid`
    /// column migration) read back as `None`. `#[serde(default)]` keeps older
    /// JSON payloads (which omit the field) deserializable.
    #[serde(default)]
    pub pid: Option<i64>,
    /// Host identifier of the machine that registered this peer (see
    /// `config::this_host`). A PID is only meaningful on the host that owns it,
    /// so liveness probing is gated on `host == this_host()`; a remote peer
    /// fails *open* (TTL-only) since we cannot probe its PID. Additive +
    /// backward-compatible: pre-existing rows (and DBs created before the `host`
    /// column migration) read back as `""`. `#[serde(default)]` keeps older JSON
    /// payloads (which omit the field) deserializable.
    #[serde(default)]
    pub host: String,
    /// Repository name (basename of the git toplevel) of this session's cwd at
    /// registration. A descriptive tag attributing the session to its physical
    /// git checkout (never an injection target, never injected text). Empty when
    /// the cwd is not a git repo or git acquisition failed. Additive +
    /// backward-compatible: DBs created before the `repo` column migration read
    /// back as `""`. `#[serde(default)]` keeps older JSON payloads (which omit
    /// the field) deserializable.
    #[serde(default)]
    pub repo: String,
    /// Current git branch (`rev-parse --abbrev-ref HEAD`) of this session's cwd
    /// at registration. Descriptive tag; empty when detached/non-git/failure.
    /// Additive + backward-compatible (see [`Peer::repo`]).
    #[serde(default)]
    pub branch: String,
    /// Canonical, path-stable worktree id: the `<name>` segment of
    /// `.git/worktrees/<name>` for a linked worktree, or the literal `"(main)"`
    /// sentinel for a main (non-linked) worktree. Empty when the cwd is not a git
    /// repo. Stable across path moves and restarts (unlike a checkout basename or
    /// `git worktree list`'s path column). Additive + backward-compatible.
    #[serde(default)]
    pub worktree_id: String,
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

    /// The lifecycle machine permits ONLY the three legal forward edges; every
    /// backward / self / from-terminal edge is rejected (monotonicity).
    #[test]
    fn ask_state_machine_is_monotonic() {
        use AskState::*;
        let all = [Open, Answered, Acked];
        let legal = [(Open, Answered), (Open, Acked), (Answered, Acked)];
        for &from in &all {
            for &to in &all {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition(to),
                    expected,
                    "{from:?} -> {to:?} should be {expected}"
                );
                // No self-edge is ever legal.
                if from == to {
                    assert!(
                        !from.can_transition(to),
                        "self-edge {from:?} must be rejected"
                    );
                }
            }
        }
    }

    /// `as_str`/`from_str` round-trip; an unknown state is a clean error, not a panic.
    #[test]
    fn ask_state_str_roundtrips() {
        for s in [AskState::Open, AskState::Answered, AskState::Acked] {
            assert_eq!(AskState::from_str(s.as_str()), Ok(s));
        }
        assert!(AskState::from_str("bogus").is_err());
    }

    /// A minted id is always accepted by the validator; hostile/oversized/charset
    /// violations are rejected.
    #[test]
    fn ask_id_validation() {
        let id = new_ask_id(42);
        assert!(id.starts_with("ask_42_"));
        assert!(ask_id_valid(&id));
        assert!(!ask_id_valid(""));
        assert!(!ask_id_valid("ask 1")); // space
        assert!(!ask_id_valid("ask;rm")); // shell metachar
        assert!(!ask_id_valid(&"x".repeat(MAX_ASK_ID_LEN + 1))); // oversized
        assert!(ask_id_valid(&"x".repeat(MAX_ASK_ID_LEN))); // exactly the cap
    }

    /// Distinct mints never collide for the same rowid (counter widens the tail).
    #[test]
    fn new_ask_id_is_unique_per_mint() {
        let a = new_ask_id(1);
        let b = new_ask_id(1);
        assert_ne!(a, b);
    }

    /// CONCURRENCY: many threads minting ids for the SAME rowid simultaneously
    /// never collide — the process-local `AtomicU64` nonce makes every minted id
    /// unique within a process run even when the rowid is identical and the wall
    /// clock is the same millisecond. Guards the no-`rand`-crate id scheme against a
    /// silent duplicate-id correlation bug under parallel `ask` calls.
    #[test]
    fn new_ask_id_is_unique_under_concurrency() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let mut handles = Vec::new();
        const THREADS: usize = 16;
        const PER_THREAD: usize = 500;
        for _ in 0..THREADS {
            let seen = Arc::clone(&seen);
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::with_capacity(PER_THREAD);
                for _ in 0..PER_THREAD {
                    // Same rowid across ALL threads: the nonce is the only thing
                    // that can keep these distinct.
                    local.push(new_ask_id(7));
                }
                let mut g = seen.lock().unwrap();
                for id in local {
                    assert!(
                        g.insert(id.clone()),
                        "duplicate correlation id minted: {id}"
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            THREADS * PER_THREAD,
            "every concurrently-minted id is unique"
        );
    }

    #[test]
    fn ask_role_parses() {
        assert_eq!(AskRole::parse("asker"), AskRole::Asker);
        assert_eq!(AskRole::parse("ASKEE"), AskRole::Askee);
        assert_eq!(AskRole::parse(""), AskRole::Any);
        assert_eq!(AskRole::parse("garbage"), AskRole::Any);
    }

    // ---- proptest: lifecycle monotonicity + correlation-id validity totality ----

    use proptest::prelude::*;

    /// Map a small index onto an `AskState` so proptest can generate transition
    /// sequences over the whole (finite) state set.
    fn state_of(i: u8) -> AskState {
        match i % 3 {
            0 => AskState::Open,
            1 => AskState::Answered,
            _ => AskState::Acked,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// MONOTONICITY TOTALITY: for EVERY ordered pair of states the lifecycle
        /// machine permits exactly the three legal forward edges and NOTHING else —
        /// never a backward edge, never a self-edge, never a move out of the
        /// terminal `Acked` state. This is the pure invariant the store consults
        /// before every UPDATE; if it ever admitted an illegal edge the lifecycle
        /// could move backward.
        #[test]
        fn ask_transition_only_three_legal_edges(a in 0u8..3, b in 0u8..3) {
            let from = state_of(a);
            let to = state_of(b);
            let legal = matches!(
                (from, to),
                (AskState::Open, AskState::Answered)
                    | (AskState::Open, AskState::Acked)
                    | (AskState::Answered, AskState::Acked)
            );
            prop_assert_eq!(from.can_transition(to), legal);
            // A self-edge is never legal.
            if from == to {
                prop_assert!(!from.can_transition(to));
            }
            // Acked is terminal: no edge leaves it.
            if from == AskState::Acked {
                prop_assert!(!from.can_transition(to));
            }
            // No edge ever moves *to* Open (no resurrection).
            if to == AskState::Open {
                prop_assert!(!from.can_transition(to));
            }
        }

        /// MONOTONICITY (path totality): NO sequence of transitions starting at
        /// `Open` and following only legal edges can ever revisit `Open` or move
        /// out of `Acked`. We walk an arbitrary index sequence, only taking legal
        /// edges, and assert the visited rank is non-decreasing (Open<Answered<Acked)
        /// and Acked is absorbing.
        #[test]
        fn ask_lifecycle_never_moves_backward(steps in proptest::collection::vec(0u8..3, 0..24)) {
            fn rank(s: AskState) -> u8 {
                match s {
                    AskState::Open => 0,
                    AskState::Answered => 1,
                    AskState::Acked => 2,
                }
            }
            let mut cur = AskState::Open;
            for &s in &steps {
                let next = state_of(s);
                if cur.can_transition(next) {
                    prop_assert!(rank(next) > rank(cur), "a legal edge must strictly advance rank");
                    cur = next;
                }
                // Once acked, no further edge is ever legal (absorbing terminal).
                if cur == AskState::Acked {
                    prop_assert!(!cur.can_transition(state_of(s)));
                }
            }
        }

        /// CORRELATION-ID VALIDITY TOTALITY: `ask_id_valid` never panics on
        /// arbitrary input and its verdict matches the documented contract exactly
        /// (non-empty, ≤ MAX_ASK_ID_LEN bytes, ASCII `[A-Za-z0-9_]` only). A
        /// metachar/oversized/empty id is always rejected before any DB bind.
        #[test]
        fn ask_id_valid_is_total(s in ".*") {
            let got = ask_id_valid(&s);
            let expect = !s.is_empty()
                && s.len() <= MAX_ASK_ID_LEN
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            prop_assert_eq!(got, expect);
            // A hostile id carrying shell metacharacters is NEVER accepted.
            if s.bytes().any(|b| matches!(b, b';' | b'|' | b'&' | b'$' | b'`' | b' ' | b'\n' | b'\'' | b'"')) {
                prop_assert!(!got);
            }
        }

        /// Every minted id (for any rowid) is accepted by the validator and carries
        /// the documented `ask_<rowid>_` prefix — the mint never produces an id its
        /// own validator would reject.
        #[test]
        fn new_ask_id_always_valid(rowid in 0i64..1_000_000) {
            let id = new_ask_id(rowid);
            prop_assert!(ask_id_valid(&id));
            let prefix = format!("ask_{rowid}_");
            prop_assert!(id.starts_with(&prefix));
        }
    }
}
