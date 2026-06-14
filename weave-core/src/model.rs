//! Core data types shared across the store, injector, and MCP layers.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Recipient aliases that mean "deliver to every session". Single source of truth.
pub const BROADCAST: &[&str] = &["all", "*", "everyone", "broadcast"];

/// WL-039: the [`Message::kind`] marker for an idle/notification "still waiting"
/// ping. Set **only** on the notify dedup path; the `supersede_prior_idle` query
/// scopes the auto-supersede to `kind = KIND_IDLE` so dedup can never touch a real
/// message. An internal enum literal, never user-supplied text.
pub const KIND_IDLE: &str = "idle";

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

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// The three descriptive session tags captured from a cwd's git state. Pure data
/// (no I/O); the store bounds each field on write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeTags {
    /// Repo name = basename of the git toplevel. Empty when non-git / unknown.
    pub repo: String,
    /// Current branch (`rev-parse --abbrev-ref HEAD`). Empty when detached /
    /// non-git / unknown.
    pub branch: String,
    /// Canonical worktree id: the `<name>` of `.git/worktrees/<name>` for a linked
    /// worktree, the `"(main)"` sentinel for a main worktree, or `""` when the cwd
    /// is not a git repo at all.
    pub worktree_id: String,
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
    /// Per-message idempotency key. Globally unique: a duplicate key anywhere in
    /// the store returns the existing message id instead of creating a new row.
    /// `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Distributed trace id for end-to-end debugging across stores and backends.
    /// Auto-minted by CLI/MCP when not provided by the caller.
    /// `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// Message priority: low, normal, high, urgent. Default normal.
    /// Additive + backward-compatible: pre-existing rows read back as "normal".
    #[serde(default = "default_priority")]
    pub priority: String,
    /// WL-037: id of the message that SUPERSEDES this one, if any. `None` (the
    /// default) means this message has not been replaced. A non-`None` value marks
    /// this row as a predecessor in a supersede/successor chain — replacement,
    /// distinct from `in_reply_to` threading: readers hide it from the unread inbox
    /// but keep it (flagged) in history/thread/search. Additive +
    /// backward-compatible: pre-existing rows (and DBs created before the
    /// `superseded_by` column migration) read back as `None`. `#[serde(default)]`
    /// keeps older JSON payloads (which omit the field) deserializable.
    #[serde(default)]
    pub superseded_by: Option<i64>,
    /// WL-038: absolute epoch-seconds deadline after which this message is
    /// **ephemeral** and is deleted on the next sweep (delete-on-sweep). `None`
    /// (the default) means the message is permanent. Stored as the absolute
    /// deadline (`ts + ttl`), not the relative ttl, so every sweep is a single
    /// `WHERE expires_at <= now()` (the `leases.expires`/`sweep_expired_leases`
    /// precedent). Additive + backward-compatible: pre-existing rows (and DBs
    /// created before the `expires_at` column migration) read back as `None`.
    /// `#[serde(default)]` keeps older JSON payloads (which omit the field)
    /// deserializable.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// WL-039: message kind marker. `None`/`"normal"` (the default) is an ordinary
    /// message; [`KIND_IDLE`] marks an idle/notification "still waiting" ping set
    /// **only** on the notify dedup path. The marker exists so idle-notification
    /// dedup (`Store::supersede_prior_idle`) can ever fire ONLY on idle pings and
    /// never on real content — it scopes the supersede `UPDATE` to `kind='idle'`.
    /// `kind` is an internal enum literal, never free user text. Additive +
    /// backward-compatible: pre-existing rows (and DBs created before the `kind`
    /// column migration) read back as `None`. `#[serde(default)]` keeps older JSON
    /// payloads (which omit the field) deserializable.
    #[serde(default)]
    pub kind: Option<String>,
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
    /// Idempotency key carried on cross-store intents so the receiver's commit
    /// is idempotent. `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Trace id carried on cross-store intents for end-to-end debugging.
    /// `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// Message priority carried on cross-store intents.
    #[serde(default = "default_priority")]
    pub priority: String,
    /// WL-038: ephemeral TTL (relative seconds) carried on cross-store intents.
    /// `0` (the default) means no TTL (permanent). The receiver re-stamps `ts` on
    /// commit, so the relative ttl — not an absolute deadline — is carried; the
    /// receiver computes `expires_at = now() + ttl` at commit (the priority/WL-031
    /// carry precedent). `#[serde(default)]` keeps older JSON payloads
    /// deserializable.
    #[serde(default)]
    pub ttl: i64,
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

/// The structured kind of a tracked ask (WL-015). Stored as TEXT in `asks.kind`;
/// `FreeText` is the default for every legacy/pre-WL-015 row. New variants can be
/// added without a schema migration because the column is free-form TEXT validated
/// at the store seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// Plain free-text question (today's default).
    #[default]
    FreeText,
    /// Multiple-choice question; options are stored newline-separated in
    /// `asks.options`.
    Choice,
    /// Tool-use permission request; `options` holds `tool_name\ntool_args`.
    ToolPermission,
}

impl AskKind {
    /// Canonical label stored in `asks.kind`. The only inlined SQL literals are
    /// derived from this (compile-time constants, never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            AskKind::FreeText => "free_text",
            AskKind::Choice => "choice",
            AskKind::ToolPermission => "tool_permission",
        }
    }

    /// Parse a stored kind string. An unknown value falls back to `FreeText` so a
    /// corrupt/foreign row degrades gracefully rather than blocking reads.
    pub fn from_str(s: &str) -> Self {
        match s {
            "choice" => AskKind::Choice,
            "tool_permission" => AskKind::ToolPermission,
            _ => AskKind::FreeText,
        }
    }

    /// Parse a caller-supplied kind string; empty/unknown defaults to `FreeText`
    /// (the safest superset — never narrows unexpectedly).
    pub fn parse(s: &str) -> Self {
        Self::from_str(s.trim().to_ascii_lowercase().as_str())
    }
}

/// WL-031: message priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessagePriority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl MessagePriority {
    pub fn as_str(self) -> &'static str {
        match self {
            MessagePriority::Low => "low",
            MessagePriority::Normal => "normal",
            MessagePriority::High => "high",
            MessagePriority::Urgent => "urgent",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => MessagePriority::Low,
            "high" => MessagePriority::High,
            "urgent" => MessagePriority::Urgent,
            _ => MessagePriority::Normal,
        }
    }
    /// Numeric rank for filtering: higher = more important.
    pub fn rank(self) -> u8 {
        match self {
            MessagePriority::Low => 0,
            MessagePriority::Normal => 1,
            MessagePriority::High => 2,
            MessagePriority::Urgent => 3,
        }
    }
}

pub fn default_priority() -> String {
    MessagePriority::Normal.as_str().to_string()
}

/// WL-032: per-peer contact policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContactPolicy {
    /// Accept all messages (default).
    #[default]
    Open,
    /// Accept from known contacts; unknown senders get an auto-approval ask.
    Auto,
    /// Accept only from explicitly allowed contacts; block others.
    ContactsOnly,
    /// Block all incoming messages.
    BlockAll,
}

impl ContactPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ContactPolicy::Open => "open",
            ContactPolicy::Auto => "auto",
            ContactPolicy::ContactsOnly => "contacts_only",
            ContactPolicy::BlockAll => "block_all",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "open" => ContactPolicy::Open,
            "auto" => ContactPolicy::Auto,
            "contacts_only" | "contacts-only" | "contactsonly" => ContactPolicy::ContactsOnly,
            "block_all" | "block-all" | "blockall" => ContactPolicy::BlockAll,
            _ => ContactPolicy::Open,
        }
    }
}

pub fn default_contact_policy() -> String {
    ContactPolicy::Open.as_str().to_string()
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
    /// Structured kind of this ask (WL-015). Defaults to `FreeText` for legacy rows.
    #[serde(default)]
    pub kind: AskKind,
    /// Kind-specific payload: newline-separated choices for `Choice`, or
    /// `tool_name\ntool_args` for `ToolPermission`. `None` for `FreeText`.
    #[serde(default)]
    pub options: Option<String>,
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
    /// Parent ask-many group id (`askm_<seed>_<nonce>`) this ask is a child of, or
    /// `None` for a standalone `ask` / a legacy P1-era row. Additive nullable column
    /// (the `in_reply_to`/`Peer` precedent); `#[serde(default)]` keeps older JSON
    /// payloads deserializable.
    #[serde(default)]
    pub parent_id: Option<String>,
}

/// Hard upper bound (in chars) on an ask-many PARENT (group) id. The id is always
/// server-minted (`askm_<seed>_<nonce>`), so it can never legitimately be long; the
/// cap rejects a hostile/oversized user-supplied parent id on `ask_many_result`
/// before it is bound into a query, the `MAX_ASK_ID_LEN` analog. 80 is more than any
/// minted id needs (`askm_` prefix + two integer tails).
pub const MAX_ASK_MANY_ID_LEN: usize = 80;

/// Validate an ask-many parent (group) id. Accepts only the minted shape:
/// non-empty, `<= MAX_ASK_MANY_ID_LEN` chars, ASCII `[A-Za-z0-9_]` only, and the
/// `askm_` prefix (so a plain `ask_<...>` child id can never be mistaken for a
/// parent). The `ask_id_valid` analog for parent ids — guards every store/MCP/CLI
/// path taking a user-supplied parent id so a metachar/oversized value is rejected
/// before any DB bind (defense-in-depth; never reaches a shell).
pub fn ask_many_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ASK_MANY_ID_LEN
        && id.starts_with("askm_")
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Mint an opaque parent id for a freshly-opened ask-many group: `askm_<seed>_<n>`
/// where `<n>` is a process-local nonce derived from `now()` + the same monotonic
/// [`ASK_NONCE`] counter `new_ask_id` uses (NO `rand`/date dependency). `seed` is the
/// `ask_groups` insertion `now()` (or any fresh integer); the nonce widens the opaque
/// tail. The result always satisfies [`ask_many_id_valid`] (digits + `_` only, `askm_`
/// prefix).
pub fn new_ask_many_id(seed: i64) -> String {
    let n = ASK_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce = (now() as u64).wrapping_mul(2_654_435_761).wrapping_add(n);
    format!("askm_{seed}_{nonce}")
}

/// The canonical question + opener of an ask-many group (the parent anchor stored in
/// the `ask_groups` table). Holds the question text/subject/opener and the post-dedup
/// `target_count` once, so totality (`answered+acked+pending+failed == target_count`)
/// is checkable even when some children failed pre-insert. Pure data, no I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskGroup {
    pub parent_id: String,
    pub asker: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub body: String,
    pub opened_ts: i64,
    pub target_count: i64,
}

/// One child row in an aggregated ask-many result: the target peer, the child's
/// correlation id (`None` if the child failed to create), its lifecycle state, the
/// answer message id (if answered), and a per-child best-effort error (the reason a
/// child failed pre-insert). Pure data assembled by the store at read time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskManyChildView {
    pub peer: String,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub state: Option<AskState>,
    #[serde(default)]
    pub answer_msg_id: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Aggregate state of an ask-many group, derived from its children at READ time (no
/// background ticker): `Complete` when no child is pending; `Partial` when some child
/// is still pending AND the caller-supplied age threshold has elapsed; else `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskManyState {
    Pending,
    Partial,
    Complete,
}

impl AskManyState {
    pub fn as_str(self) -> &'static str {
        match self {
            AskManyState::Pending => "pending",
            AskManyState::Partial => "partial",
            AskManyState::Complete => "complete",
        }
    }
}

/// Classify an ask-many group's aggregate state from its child rollup — PURE (no
/// I/O), so it is the unit/proptest target. `Complete` iff no child is still pending
/// (`pending == 0`, every child answered/acked/failed). Otherwise `Partial` only when
/// the caller passed an `age_threshold` AND the group's age (`age_secs`) has reached
/// it (daemon-free, opt-in timeout); else `Pending`. Totality is the caller's
/// invariant: `answered + acked + pending + failed == total`.
pub fn classify_ask_many(
    _total: i64,
    pending: i64,
    _failed: i64,
    age_secs: Option<i64>,
    age_threshold: Option<i64>,
) -> AskManyState {
    if pending <= 0 {
        return AskManyState::Complete;
    }
    if let (Some(age), Some(thr)) = (age_secs, age_threshold) {
        if thr > 0 && age >= thr {
            return AskManyState::Partial;
        }
    }
    AskManyState::Pending
}

/// The full aggregated read-time view of an ask-many group: the parent question +
/// opener + target count, the rollup counts, the derived [`AskManyState`], and the
/// per-child views. Pure data returned by `Store::ask_many_result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskManyResult {
    pub parent_id: String,
    pub asker: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub body: String,
    pub opened_ts: i64,
    pub target_count: i64,
    pub total: i64,
    pub answered: i64,
    pub acked: i64,
    pub pending: i64,
    pub failed: i64,
    pub state: AskManyState,
    pub children: Vec<AskManyChildView>,
}

/// The resolved status of a ToolPermission ask (WL-021). Derived at read time
/// from the ask state, answer body, and age. Pure; no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionStatus {
    /// Awaiting an answer; within the timeout window.
    Pending,
    /// The askee answered with body "approve" (case-insensitive, trimmed).
    Approved,
    /// The askee answered with anything other than "approve", or the ask was
    /// explicitly denied.
    Denied,
    /// Still open but older than the timeout — treated as denied by default.
    Timeout,
}

impl PermissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionStatus::Pending => "pending",
            PermissionStatus::Approved => "approved",
            PermissionStatus::Denied => "denied",
            PermissionStatus::Timeout => "timeout",
        }
    }
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(PermissionStatus::Pending),
            "approved" => Ok(PermissionStatus::Approved),
            "denied" => Ok(PermissionStatus::Denied),
            "timeout" => Ok(PermissionStatus::Timeout),
            other => Err(format!("unknown permission status '{other}'")),
        }
    }
}

/// Default timeout for ToolPermission asks (seconds). After this window an
/// unanswered permission ask is treated as [`PermissionStatus::Timeout`].
pub const PERMISSION_TIMEOUT_SECS: i64 = 300;

/// Resolve the permission status of an ask at read time. Requires the answer
/// body (when present) and the current wall-clock time. Totality: never panics.
pub fn permission_status(
    ask: &Ask,
    answer_body: Option<&str>,
    now: i64,
    timeout_secs: i64,
) -> PermissionStatus {
    // Terminal states (answered or acked) → inspect the answer body.
    if ask.state == AskState::Answered || ask.state == AskState::Acked {
        if let Some(body) = answer_body {
            if body.trim().eq_ignore_ascii_case("approve") {
                return PermissionStatus::Approved;
            }
        }
        return PermissionStatus::Denied;
    }
    // Still open → check age against timeout.
    if now.saturating_sub(ask.opened_ts) >= timeout_secs {
        PermissionStatus::Timeout
    } else {
        PermissionStatus::Pending
    }
}

// ──────────────────────────────────────────────────────────────────────────
// P3 — Job board (poll-only, daemon-free). A durable work queue: a creator mints
// a `queued` job, a worker CLAIMS it (minting an `attempt_id` claim token),
// updates its lifecycle (fenced by that token), and posts a terminal result.
// There is NO autonomous dispatch/runner here (that is P10/P11): nothing nudges,
// nothing spawns. Pure model state machine + ids; the lifecycle lives in `store`.
// ──────────────────────────────────────────────────────────────────────────

/// Hard upper bound (in chars) on a job id. The id is always server-minted
/// (`job_<seed>_<nonce>`), so it can never legitimately be long; the cap rejects a
/// hostile/oversized user-supplied REFERENCE id (on show/update/result/cancel)
/// before it is bound into a query — the `MAX_ASK_ID_LEN` analog. 80 is generous
/// (`job_` prefix + two integer tails).
pub const MAX_JOB_ID_LEN: usize = 80;

/// Hard upper bound (in chars) on a job attempt (claim) id. Server-minted
/// (`att_<seed>_<nonce>`); the cap rejects a hostile/oversized supplied token on
/// `update` before any bind. 80 mirrors [`MAX_JOB_ID_LEN`].
pub const MAX_ATTEMPT_ID_LEN: usize = 80;

/// Hard upper bound (in chars) on a job's free-text fields (title, description,
/// prompt, phase, notes, reasons, summaries). Job text is echoed into other
/// agents' listings/contexts, so an unbounded one is a token/RAM/UI hazard — the
/// [`crate::store::MAX_BODY`]-class cap, applied at the job text seams.
pub const MAX_JOB_TEXT: usize = 65_536;

/// Hard upper bound (in BYTES) on a stored job JSON payload (result/error/
/// artifacts). These are peer-supplied opaque blobs persisted as TEXT; an
/// unbounded one is a disk + token/RAM DoS once re-rendered into another agent's
/// context. Enforced at the store write seam so CLI + MCP are both covered.
pub const MAX_JOB_JSON: usize = 65_536;

/// Validate a job id. Accepts only the minted shape: non-empty, `<= MAX_JOB_ID_LEN`
/// chars, ASCII `[A-Za-z0-9_]` only, and the `job_` prefix (so an `att_`/`ask_` id
/// can never be mistaken for a job id). Guards every store/MCP/CLI path taking a
/// user-supplied job id so a metachar/oversized value is rejected before any DB
/// bind (defense-in-depth; never reaches a shell).
pub fn job_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_JOB_ID_LEN
        && id.starts_with("job_")
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Validate a job attempt (claim) id. Accepts only the minted shape: non-empty,
/// `<= MAX_ATTEMPT_ID_LEN` chars, ASCII `[A-Za-z0-9_]` only, and the `att_` prefix.
/// The `job_id_valid` analog for the fencing token.
pub fn attempt_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ATTEMPT_ID_LEN
        && id.starts_with("att_")
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Mint an opaque job id for a freshly-inserted jobs row: `job_<seed>_<n>` where
/// `<n>` is a nonce derived from `now()`, the process id, AND the SAME monotonic
/// [`ASK_NONCE`] counter the ask ids use (NO `rand`/date dependency). Unlike asks
/// (which seed from a fresh AUTOINCREMENT message rowid), a job has no pre-insert
/// integer anchor, so the nonce MUST be unique across SEPARATE processes too — each
/// `weave job create` is its own process whose counter resets to 0. Mixing in the
/// PID makes two same-second, counter-0 mints in different processes diverge, so the
/// `jobs.id` PRIMARY KEY never collides. Always satisfies [`job_id_valid`].
pub fn new_job_id(seed: i64) -> String {
    format!("job_{seed}_{}", mint_nonce(2_246_822_519))
}

/// Mint an opaque attempt (claim) token for a job CLAIM: `att_<seed>_<n>`, same
/// cross-process-unique nonce scheme as [`new_job_id`]. Re-claiming a job mints a
/// fresh token (the monotonic counter guarantees it differs within a process; the
/// PID/clock guarantee it across processes), which is what fences out a prior
/// worker's now-stale token in `update_job`. Always satisfies [`attempt_id_valid`].
pub fn new_attempt_id(seed: i64) -> String {
    format!("att_{seed}_{}", mint_nonce(3_266_489_917))
}

/// Build a digits-only nonce tail unique across processes AND within a process:
/// mixes the wall clock, the OS process id, and the monotonic [`ASK_NONCE`] counter
/// (NO `rand`/date crate). `mul` is a per-call-site odd multiplier so a job id and
/// an attempt id minted in the same instant still differ.
fn mint_nonce(mul: u64) -> u64 {
    let n = ASK_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    (now() as u64)
        .wrapping_mul(mul)
        .wrapping_add((std::process::id() as u64).wrapping_mul(2_654_435_761))
        .wrapping_add(n)
}

/// Lifecycle state of a board job. The 11-variant set is the FULL repowire
/// `WorkState` vocabulary kept for forward-compat + totality (so a future
/// autonomous-dispatch epic needs NO model migration). The P3 write paths only
/// ever MINT the poll-only subset (`Queued` on create; `Running`/`AwaitingInput`/
/// `Blocked`/`Completed`/`Failed`/`Cancelled`/`Expired` via claim/update/cancel);
/// `Dispatching`/`Delivered`/`Unavailable` are runner phases — accepted on read
/// (legacy/foreign rows) and reachable via a generic `update_job` if a caller
/// insists, but no P3 code path produces them. Stored as TEXT and validated
/// through this enum (the `AskState` precedent). The machine is monotonic-ish: no
/// edge OUT of a terminal state (see [`JobState::can_transition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Dispatching,
    Delivered,
    Running,
    AwaitingInput,
    Completed,
    Failed,
    Cancelled,
    Blocked,
    Expired,
    Unavailable,
}

impl JobState {
    /// Canonical lowercase label stored in the `jobs.state` TEXT column. The only
    /// inlined SQL "literals" for job state are derived from this (compile-time
    /// constants, never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Dispatching => "dispatching",
            JobState::Delivered => "delivered",
            JobState::Running => "running",
            JobState::AwaitingInput => "awaiting_input",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Blocked => "blocked",
            JobState::Expired => "expired",
            JobState::Unavailable => "unavailable",
        }
    }

    /// Parse a stored state string back into the enum. An unknown value is a hard
    /// error at the store mapper (never a panic, never silently coerced) so a
    /// corrupt/foreign row surfaces loudly rather than mis-driving the machine.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "queued" => Ok(JobState::Queued),
            "dispatching" => Ok(JobState::Dispatching),
            "delivered" => Ok(JobState::Delivered),
            "running" => Ok(JobState::Running),
            "awaiting_input" => Ok(JobState::AwaitingInput),
            "completed" => Ok(JobState::Completed),
            "failed" => Ok(JobState::Failed),
            "cancelled" => Ok(JobState::Cancelled),
            "blocked" => Ok(JobState::Blocked),
            "expired" => Ok(JobState::Expired),
            "unavailable" => Ok(JobState::Unavailable),
            other => Err(format!("unknown job state '{other}'")),
        }
    }

    /// The TERMINAL set: a job in one of these states is DONE and may not change
    /// state again (poll-only retry == create a NEW job). Frozen at
    /// `{Completed, Failed, Cancelled, Expired, Unavailable}` — the recommended
    /// coherent poll-only interpretation (repowire re-acquires `unavailable` only
    /// via its runner, which P3 does not have).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed
                | JobState::Failed
                | JobState::Cancelled
                | JobState::Expired
                | JobState::Unavailable
        )
    }

    /// Legal forward edges of the lifecycle machine — PURE (no I/O), so it is the
    /// unit/proptest target; the store consults it before every state UPDATE.
    /// Rules: (1) a terminal state is FROZEN — the only allowed "edge" is the
    /// idempotent self-noop (`self == to`); (2) cancellation/expiry may INTERRUPT
    /// from ANY non-terminal state; (3) otherwise progress moves forward within the
    /// active lane (a self-edge to the same non-terminal state is allowed as an
    /// idempotent re-write). No edge ever moves OUT of a terminal state.
    pub fn can_transition(self, to: JobState) -> bool {
        if self.is_terminal() {
            return self == to; // terminal frozen; same-terminal re-write is a no-op
        }
        if to == JobState::Cancelled || to == JobState::Expired {
            return true; // cancel/expire interrupt any non-terminal state
        }
        matches!(
            (self, to),
            (
                JobState::Queued,
                JobState::Queued
                    | JobState::Dispatching
                    | JobState::Delivered
                    | JobState::Running
                    | JobState::AwaitingInput
                    | JobState::Blocked
                    | JobState::Completed
                    | JobState::Failed
                    | JobState::Unavailable
            ) | (
                JobState::Dispatching,
                JobState::Dispatching
                    | JobState::Delivered
                    | JobState::Running
                    | JobState::Failed
                    | JobState::Unavailable
            ) | (
                JobState::Delivered,
                JobState::Delivered
                    | JobState::Running
                    | JobState::AwaitingInput
                    | JobState::Blocked
                    | JobState::Completed
                    | JobState::Failed
                    | JobState::Unavailable
            ) | (
                JobState::Running,
                JobState::Running
                    | JobState::AwaitingInput
                    | JobState::Blocked
                    | JobState::Completed
                    | JobState::Failed
                    | JobState::Unavailable
            ) | (
                JobState::AwaitingInput,
                JobState::AwaitingInput
                    | JobState::Running
                    | JobState::Blocked
                    | JobState::Completed
                    | JobState::Failed
                    | JobState::Unavailable
            ) | (
                JobState::Blocked,
                JobState::Blocked
                    | JobState::Running
                    | JobState::AwaitingInput
                    | JobState::Completed
                    | JobState::Failed
                    | JobState::Unavailable
            )
        )
    }
}

/// Which jobs a `list_jobs` query keeps. Each field is an optional exact-match
/// filter (`None` == unconstrained); a populated `state` further narrows by
/// lifecycle. Pure data (no I/O), shared by the store + the mcp/main consumers.
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub state: Option<JobState>,
    pub owner: Option<String>,
    pub creator: Option<String>,
    pub assignee: Option<String>,
    pub circle: Option<String>,
}

/// The create-time spec for a new board job. An owned struct (not a long argv) so
/// the store signature stays small and additive-friendly — new inert board fields
/// can be added without churning every call site. `title` is the only required
/// field; everything else defaults (`owner` ⇒ creator, `kind` ⇒ "general",
/// `visibility` ⇒ "circle"). The runner-only knobs (cron/schedule/spawn-exec) are
/// deliberately ABSENT (P10/P11).
#[derive(Debug, Clone, Default)]
pub struct JobSpec {
    pub title: String,
    pub description: Option<String>,
    pub kind: Option<String>,
    pub owner: Option<String>,
    pub assignee: Option<String>,
    pub circle: Option<String>,
    pub prompt: Option<String>,
    pub correlation_id: Option<String>,
    pub source_kind: Option<String>,
    pub source_id: Option<String>,
    pub scope: Option<String>,
    pub visibility: Option<String>,
    pub deadline_at: Option<i64>,
    pub expires_at: Option<i64>,
}

/// A mutation patch applied by `update_job`. Every field is optional — only the
/// `Some` fields are written. `state` (when present) is guarded by
/// [`JobState::can_transition`]; a `progress_note` is APPENDED to the append-only
/// event log (never overwrites). Pure data, shared by store + consumers.
#[derive(Debug, Clone, Default)]
pub struct JobPatch {
    pub state: Option<JobState>,
    pub state_reason: Option<String>,
    pub phase: Option<String>,
    pub progress_note: Option<String>,
    pub result_summary: Option<String>,
    pub result_json: Option<String>,
    pub error_json: Option<String>,
    pub artifacts_json: Option<String>,
}

/// A board job (P3). The mutable lifecycle row of the work queue. Same
/// `#[serde(default)]` discipline as [`Ask`] on the nullable/added fields so older
/// JSON payloads stay deserializable. Timestamps are weave-native `i64` epoch secs
/// ([`now`]), NOT ISO strings (the no-date-crate discipline; asks do the same).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Opaque id (`job_<seed>_<nonce>`); the PK.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: String,
    pub state: JobState,
    #[serde(default)]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub progress_note: Option<String>,
    /// Append-only event log JSON (`[{at,note,state,phase}]`).
    #[serde(default)]
    pub progress_events_json: String,
    pub creator: String,
    #[serde(default)]
    pub owner: Option<String>,
    /// Set on CLAIM (the worker that holds the active attempt).
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub circle: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub source_kind: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub visibility: String,
    /// Current claim token (`att_<...>`); `None` until claimed. The fencing key.
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub deadline_at: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub result_summary: Option<String>,
    #[serde(default)]
    pub result_json: String,
    #[serde(default)]
    pub error_json: String,
    #[serde(default)]
    pub artifacts_json: String,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub cancel_requested_by: Option<String>,
    #[serde(default)]
    pub cancel_requested_ts: Option<i64>,
    #[serde(default)]
    pub cancel_reason: Option<String>,
    pub opened_ts: i64,
    pub updated_ts: i64,
    /// Stamped on entry to any terminal state.
    #[serde(default)]
    pub completed_ts: Option<i64>,
}

/// The read-time result view of a job (`job_result`). When the job is terminal it
/// carries the terminal payload; otherwise `ready` is false and the rest is the
/// not-ready marker (mirrors repowire `tracked_work.result()`). Pure data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResultView {
    pub id: String,
    pub state: JobState,
    pub ready: bool,
    #[serde(default)]
    pub result_summary: Option<String>,
    pub result_json: String,
    pub error_json: String,
    pub artifacts_json: String,
    #[serde(default)]
    pub completed_ts: Option<i64>,
}

// ---- WL-020: review queue ----

/// Hard upper bound (in chars) on a review item id. `review_<seed>_<nonce>`.
pub const MAX_REVIEW_ID_LEN: usize = 80;

/// Hard upper bound (in chars) on a review item title.
pub const MAX_REVIEW_TITLE_LEN: usize = 256;

/// Hard upper bound (in chars) on a review item author/repo.
pub const MAX_REVIEW_IDENT_LEN: usize = 64;

/// WL-024: max resource string length for a lease reservation.
pub const MAX_LEASE_RESOURCE_LEN: usize = 512;
/// WL-024: max note length for a lease reservation.
pub const MAX_LEASE_NOTE_LEN: usize = 1024;
/// WL-024: max TTL for a lease in seconds (≈ 24 hours).
pub const MAX_LEASE_TTL_SECS: i64 = 86_400;

/// WL-038: max TTL for an ephemeral message in seconds (≈ 24 hours). Mirrors the
/// lease ceiling; bounding the TTL also prevents an `expires_at = ts + ttl`
/// overflow/abuse (the cap + `expiry_from_ttl`'s `saturating_add` together).
pub const MAX_MSG_TTL_SECS: i64 = 86_400;

/// The lifecycle state of a PR in the review queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewItemState {
    #[default]
    Open,
    Merged,
    Closed,
}

impl ReviewItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewItemState::Open => "open",
            ReviewItemState::Merged => "merged",
            ReviewItemState::Closed => "closed",
        }
    }
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "open" => Ok(ReviewItemState::Open),
            "merged" => Ok(ReviewItemState::Merged),
            "closed" => Ok(ReviewItemState::Closed),
            other => Err(format!("unknown review state '{other}'")),
        }
    }
}

/// Filter for review queue listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewQueueFilter {
    #[default]
    All,
    Open,
    Pending,
    Reviewed,
}

impl ReviewQueueFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewQueueFilter::All => "all",
            ReviewQueueFilter::Open => "open",
            ReviewQueueFilter::Pending => "pending",
            ReviewQueueFilter::Reviewed => "reviewed",
        }
    }
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(ReviewQueueFilter::All),
            "open" => Ok(ReviewQueueFilter::Open),
            "pending" => Ok(ReviewQueueFilter::Pending),
            "reviewed" => Ok(ReviewQueueFilter::Reviewed),
            other => Err(format!("unknown review filter '{other}'")),
        }
    }
}

/// A single PR review item tracked across peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub pr_url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub repo: String,
    pub state: ReviewItemState,
    #[serde(default)]
    pub review_requested_at: Option<i64>,
    #[serde(default)]
    pub reviewed_at: Option<i64>,
    #[serde(default)]
    pub reviewed_by: Option<String>,
    pub created_at: i64,
}

/// Validate a review id: `review_<seed>_<nonce>`.
pub fn review_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REVIEW_ID_LEN
        && id.starts_with("review_")
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Hard upper bound (in chars) on an idempotency key. 128 is generous for
/// caller-minted keys (UUIDs, ULIDs, or namespaced application keys).
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

/// Hard upper bound (in chars) on a trace id. 128 is generous for
/// caller-supplied correlation ids.
pub const MAX_TRACE_ID_LEN: usize = 128;

/// Validate an idempotency key: non-empty, <= MAX_IDEMPOTENCY_KEY_LEN chars,
/// no control characters, no NUL. Rejects overly long or hostile keys before
/// any DB bind.
pub fn idempotency_key_valid(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_IDEMPOTENCY_KEY_LEN
        && !key.bytes().any(|b| b.is_ascii_control())
        && !key.contains('\0')
}

/// Validate a trace id: non-empty, <= MAX_TRACE_ID_LEN chars, no control
/// characters, no NUL. More permissive than idempotency keys — the format is
/// caller-defined.
pub fn trace_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TRACE_ID_LEN
        && !id.bytes().any(|b| b.is_ascii_control())
        && !id.contains('\0')
}

/// Mint a trace id for end-to-end debugging: `trace_<timestamp>_<6 random hex>`.
pub fn mint_trace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut buf = [0u8; 3];
    if getrandom::getrandom(&mut buf).is_ok() {
        format!("trace_{ts}_{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2])
    } else {
        format!("trace_{ts}_000000")
    }
}

/// Validate a GitHub PR URL (basic check: https://github.com/owner/repo/pull/N).
pub fn pr_url_valid(url: &str) -> bool {
    !url.is_empty()
        && url.len() <= crate::store::MAX_BODY
        && url.starts_with("https://github.com/")
        && url.contains("/pull/")
}

/// Mint an opaque review id.
pub fn new_review_id(seed: i64) -> String {
    format!("review_{seed}_{}", mint_nonce(3_141_592_653))
}

/// WL-024: a lightweight advisory lease reservation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub resource: String,
    pub holder: String,
    pub acquired: i64,
    pub expires: i64,
    #[serde(default)]
    pub note: String,
}

/// WL-024: validate a lease resource string.
pub fn lease_resource_valid(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= MAX_LEASE_RESOURCE_LEN
        && !r.contains('\0')
        && r.chars().all(|c| !c.is_control())
}

/// WL-024: validate a lease TTL in seconds.
pub fn lease_ttl_valid(ttl: i64) -> bool {
    ttl > 0 && ttl <= MAX_LEASE_TTL_SECS
}

/// WL-038: validate an ephemeral-message TTL in seconds. Accepts `1..=MAX_MSG_TTL_SECS`;
/// rejects `0`, negatives, and over-cap values at the CLI/MCP seam (the
/// `lease_ttl_valid` precedent).
pub fn ttl_valid(ttl: i64) -> bool {
    (1..=MAX_MSG_TTL_SECS).contains(&ttl)
}

/// WL-038: compute an absolute ephemeral deadline from a base timestamp and a
/// relative ttl. Uses `saturating_add` so an `i64::MAX` base never wraps/panics
/// (callers must still validate via [`ttl_valid`] first).
pub fn expiry_from_ttl(ts: i64, ttl: i64) -> i64 {
    ts.saturating_add(ttl)
}

/// WL-029: normalize a lease resource path for conflict detection.
/// Strips trailing slashes, collapses multiple slashes, rejects `..` and empty
/// segments. Returns the normalized path (always without trailing slash).
pub fn lease_path_normalize(r: &str) -> String {
    let mut out = Vec::new();
    for seg in r.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            out.pop();
            continue;
        }
        out.push(seg);
    }
    out.join("/")
}

/// WL-029: check whether two normalized resource paths conflict.
/// Conflicts are: exact match, or one is a strict ancestor of the other
/// (prefix match followed by a path separator).
pub fn lease_path_conflicts(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_slash = format!("{a}/");
    let b_slash = format!("{b}/");
    b.starts_with(&a_slash) || a.starts_with(&b_slash)
}

/// Hard upper bound (in chars) on a circle label. A circle is a small grouping
/// tag (a visibility-scoping label), never a path or shell token, so 64 is more
/// than enough; the cap exists to reject a hostile/oversized value before it is
/// bound into a query or stored (the `MAX_IDENT`/`MAX_ASK_ID_LEN` analog).
pub const MAX_CIRCLE_LEN: usize = 64;

/// Hard upper bound (in chars) on a birth certificate nonce. The cert is 32
/// random bytes hex-encoded = exactly 64 chars. The cap rejects a hostile or
/// pasted oversized value before it is bound into a query.
pub const MAX_BIRTH_CERT_LEN: usize = 64;

/// The semantic default circle. Legacy rows, empty values, and any peer that
/// never set `WEAVE_CIRCLE`/`config.circle` classify here, so a single-circle
/// (pre-P4) deployment behaves byte-identically.
pub const DEFAULT_CIRCLE: &str = "default";

/// Validate a circle label. Accepts a non-empty, `<= MAX_CIRCLE_LEN` string of
/// ASCII `[A-Za-z0-9_-]` only — a grouping label, never a path/shell token. This
/// is the `ask_id_valid` analog (it additionally allows `-`); it guards every
/// store/MCP/CLI surface that takes a user-supplied circle so a metachar-bearing
/// or oversized value is rejected before any DB bind (defense-in-depth even
/// though all SQL is parameterized; a circle never reaches a shell).
pub fn circle_valid(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= MAX_CIRCLE_LEN
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Normalize a stored/legacy circle value: an empty string (a pre-P4 row, or a
/// stray blank) materializes the [`DEFAULT_CIRCLE`]. Defense-in-depth — the
/// column default is already `'default'`, but reads coalesce so even a blank
/// value classifies into the default circle.
pub fn circle_or_default(c: &str) -> &str {
    if c.is_empty() {
        DEFAULT_CIRCLE
    } else {
        c
    }
}

/// The coordination role of a peer within its circle. A deliberately
/// two-variant enum stored as TEXT in `peers.role` (the [`AskState`]/`JobState`
/// precedent): `Peer` (the default; a plain participant scoped to its own
/// circle) and `Orchestrator` (the single per-circle coordinator with mesh-wide
/// default visibility). Stored through `as_str`/`from_str` so a future epic can
/// ADD variants with no schema migration. `role` is an ENUM, never free text —
/// the only path to `Orchestrator` is `claim_orchestrator_role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    /// A plain participant, scoped to its own circle. The default for every
    /// fresh registration.
    #[default]
    Peer,
    /// The single coordinator of a circle; gets mesh-wide default visibility.
    Orchestrator,
}

impl PeerRole {
    /// Canonical lowercase label stored in the `peers.role` TEXT column. The only
    /// inlined SQL "literals" for role are derived from this (compile-time
    /// constants, never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRole::Peer => "peer",
            PeerRole::Orchestrator => "orchestrator",
        }
    }

    /// Parse a stored role string back into the enum. An empty value (a legacy
    /// pre-P4 row) coalesces to the default `Peer`; any other unknown value is a
    /// hard error at the store mapper (never a panic, never silently coerced) so
    /// a corrupt/foreign row surfaces loudly.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "" | "peer" => Ok(PeerRole::Peer),
            "orchestrator" => Ok(PeerRole::Orchestrator),
            other => Err(format!("unknown peer role '{other}'")),
        }
    }
}

/// The live turn-state of a peer within its current session (P5 rich presence).
/// A deliberately small enum stored as TEXT in `peers.turn_state` (the
/// [`PeerRole`]/`AskState` precedent): an empty string (`Unknown`, the column
/// default) is a legacy/pre-hook row that has never reported a state; the four
/// labels are the canonical lifecycle vocabulary (mirrors repowire's
/// `TurnState`). turn_state is hook-auto-set from `handle_hook` (zero friction)
/// and never free text — every store write validates through [`TurnState::from_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// No state reported yet (legacy/pre-hook row); the empty-string default.
    #[default]
    Unknown,
    /// A freshly-registered session that has not taken its first turn
    /// (SessionStart fired, no prompt yet).
    PendingFirstTurn,
    /// Mid-turn — a UserPromptSubmit fired and the agent is working.
    Working,
    /// The agent's prompt is live + unconsumed (Notification fired); a human or
    /// orchestrator should respond.
    AwaitingInput,
    /// The turn finished cleanly (Stop fired); no turn in progress.
    Idle,
}

impl TurnState {
    /// Canonical label stored in the `peers.turn_state` TEXT column. `Unknown` ⇒
    /// the empty string (matching the column default and the `PeerRole`/empty
    /// precedent). The only inlined SQL "literals" for turn_state are derived from
    /// this (compile-time constants, never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            TurnState::Unknown => "",
            TurnState::PendingFirstTurn => "pending_first_turn",
            TurnState::Working => "working",
            TurnState::AwaitingInput => "awaiting_input",
            TurnState::Idle => "idle",
        }
    }

    /// Parse a stored turn_state string back into the enum. An empty value (a
    /// legacy/pre-hook row) coalesces to [`TurnState::Unknown`]; any other unknown
    /// value is a hard error at the store/setter seam (never a panic, never
    /// silently coerced) so a corrupt/foreign value surfaces loudly rather than
    /// being stored raw — the [`PeerRole::from_str`]/`AskState::from_str` precedent.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "" | "unknown" => Ok(TurnState::Unknown),
            "pending_first_turn" => Ok(TurnState::PendingFirstTurn),
            "working" => Ok(TurnState::Working),
            "awaiting_input" => Ok(TurnState::AwaitingInput),
            "idle" => Ok(TurnState::Idle),
            other => Err(format!("unknown turn state '{other}'")),
        }
    }
}

/// Hard upper bound (in chars) on a peer's free-form description (P5). A
/// description is a one-line self-reported task summary echoed into other agents'
/// listings, so an unbounded one is a token/RAM/UI hazard. 200 is generous (well
/// over the git-tag caps, far under [`crate::store::MAX_BODY`]). The description
/// is control-stripped + capped via `store::sanitize_tag(_, MAX_DESC_LEN)` at the
/// store seam (lossy-but-total, internal spaces preserved).
pub const MAX_DESC_LEN: usize = 200;

/// Read-time TTL (seconds) after which a peer's free-form description ages out and
/// reads as absent (P5). Equal to [`crate::store::ONLINE_TTL_SECS`] (900s) by
/// value but kept as its own named constant so the description ages out
/// INDEPENDENTLY of liveness (a session can be alive-and-working for >900s yet its
/// description should still go stale; matches repowire's separate
/// `description_ttl_seconds`). Lives in `model` so [`expire_description`] stays
/// pure + self-contained (no I/O, no store dependency).
pub const DESCRIPTION_TTL_SECS: i64 = 900;

/// Pure, read-time TTL expiry for a peer's description (P5). If the description is
/// non-empty, anchored (`description_ts > 0`), and older than
/// [`DESCRIPTION_TTL_SECS`], blank it so every surface treats it as absent. The
/// stored row is NOT mutated — this is a read-time view; the next `set_description`
/// (or a natural overwrite) re-stamps. Daemon-free (no sweeper), the liveness-TTL
/// idiom. Totality: never panics for any `(now, description_ts)` including
/// negatives/overflow (uses `i64::saturating_sub`).
pub fn expire_description(p: &mut Peer, now: i64) {
    if !p.description.is_empty()
        && p.description_ts > 0
        && now.saturating_sub(p.description_ts) >= DESCRIPTION_TTL_SECS
    {
        p.description = String::new();
    }
}

/// serde default for [`Peer::circle`] so older JSON payloads (which omit the
/// field) deserialize to the [`DEFAULT_CIRCLE`].
fn default_circle() -> String {
    DEFAULT_CIRCLE.to_string()
}

/// The outcome of a [`claim_orchestrator_role`](crate::store::Store::claim_orchestrator_role)
/// call, rendered by the MCP/CLI surface. A `Refused` is a clean, expected verdict
/// (a live holder blocks an unforced claim), NOT an error. Pure data (no I/O).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum ClaimOutcome {
    /// The caller now holds the orchestrator role for `circle`. `demoted` lists
    /// any prior orchestrators that were demoted to `peer` in the same claim
    /// (empty on a no-contest or idempotent re-claim).
    Claimed {
        circle: String,
        demoted: Vec<String>,
    },
    /// A different LIVE orchestrator already holds the circle and `force` was not
    /// set; the claim was refused without any write. `holder` is the live holder.
    Refused { circle: String, holder: String },
}

/// The result of an [`orchestrator_status`](crate::store::Store::orchestrator_status)
/// query for a circle. `present` is true iff at least one LIVE (`role='orchestrator'`
/// AND `is_alive`) holder exists; `holders` lists all live ones. Pure data (no I/O),
/// shared by the store + the mcp/main consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorStatus {
    pub circle: String,
    pub present: bool,
    #[serde(default)]
    pub holders: Vec<Peer>,
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
    /// Visibility-scoping circle this session belongs to (P4). Captured at
    /// registration from `WEAVE_CIRCLE`/`config.circle`; defaults to
    /// [`DEFAULT_CIRCLE`]. Reads normalize empty/legacy values through
    /// [`circle_or_default`]. Additive + backward-compatible: DBs created before
    /// the `circle` column migration read back as `"default"` (the column
    /// default), and `#[serde(default = "default_circle")]` keeps older JSON
    /// payloads (which omit the field) deserializable into the default circle.
    #[serde(default = "default_circle")]
    pub circle: String,
    /// Coordination role label within the circle (P4): `"peer"` (default) or
    /// `"orchestrator"`. Stored as the TEXT label and mapped through [`PeerRole`]
    /// at the store-read seam (the `Job.visibility`-is-a-plain-`String`
    /// precedent). NEVER set at registration — only `claim_orchestrator_role`
    /// promotes a row to `"orchestrator"`. Additive + backward-compatible: legacy
    /// rows read back as `"peer"`. `#[serde(default)]` (an empty string ⇒
    /// `PeerRole::Peer`) keeps older JSON payloads deserializable.
    #[serde(default)]
    pub role: String,
    /// Live turn-state label within the current session (P5): `""` (Unknown, the
    /// default), `"pending_first_turn"`, `"working"`, `"awaiting_input"`, or
    /// `"idle"`. Stored as the TEXT label and mapped through [`TurnState`] at the
    /// surface seam (the `role`-is-a-plain-`String` precedent). Hook-auto-set from
    /// `handle_hook`; NEVER set at registration (omitted from the upsert, like
    /// `role`). Additive + backward-compatible: legacy rows read `""` (Unknown).
    /// `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub turn_state: String,
    /// Free-form, self-reported task summary (P5). Empty == none. Bounded +
    /// control-stripped via `sanitize_tag(_, MAX_DESC_LEN)` at the store seam, and
    /// TTL'd at READ time via [`expire_description`] so a stale description ages
    /// out to `""` independently of liveness. Explicit-set only (`weave describe`
    /// / `weave_set_description`); never set at registration (omitted from the
    /// upsert, like `role`). Additive + backward-compatible: legacy rows read `""`.
    #[serde(default)]
    pub description: String,
    /// Unix-seconds when [`Peer::description`] was last set (P5); `0` == never set
    /// / no TTL anchor. A SEPARATE column (not `last_seen`) so the description
    /// expires independently of liveness. Additive + backward-compatible: legacy
    /// rows read `0`. `#[serde(default)]` keeps older JSON payloads deserializable.
    #[serde(default)]
    pub description_ts: i64,
    /// Birth certificate nonce for identity takeover protection (WL-018).
    /// Nullable; None means "not yet enrolled". Additive + backward-compatible.
    #[serde(default)]
    pub birth_cert: Option<String>,
    /// Contact policy: open, auto, contacts_only, block_all. Default open.
    /// Additive + backward-compatible: pre-existing rows read back as "open".
    #[serde(default = "default_contact_policy")]
    pub contact_policy: String,
}

/// Daemon-tier liveness classification (v0.2 presence seam).  Three tiers:
/// - `Live`    — a fresh daemon heartbeat exists (≤ 30 s).
/// - `Likely`  — no fresh heartbeat, but `last_seen` is within the 900 s TTL.
/// - `Offline` — neither heartbeat nor TTL recency.
///
/// This is intentionally simpler than [`crate::store::Liveness`], which carries
/// host-aware local/remote/stale nuance.  The daemon tier is what display surfaces
/// consult when the optional presence daemon is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Likely,
    Offline,
}

impl Liveness {
    /// Canonical lowercase label for JSON / MCP output.
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Live => "live",
            Liveness::Likely => "likely",
            Liveness::Offline => "offline",
        }
    }
}

/// Hard upper bound on the number of rows a single `delivery_log` (delivery-trace)
/// read returns. The trace is append-only and bounded by retention (`gc()`), but a
/// read is additionally capped so a pathological ref can never return an unbounded
/// vector to the MCP/CLI surface — the `MAX_ASK_MANY`/inbox-limit precedent. 500 is
/// far more stages than any real delivery accrues (≤5 per message typically).
pub const MAX_DELIVERY_ROWS: i64 = 500;

/// Which kind of artifact a `delivery_log` row traces. `Message` = a plain
/// `weave_send`; `Notify` = a fire-and-forget `weave_notify`; `Ask` = a tracked
/// `weave_ask` question. Stored as TEXT (see the `delivery_log` table) and validated
/// through this enum so the store never binds raw garbage; mirrors the `AskState`
/// as_str/from_str pattern. Pure value type — DAG layer `model` (no I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryRefKind {
    Message,
    Notify,
    Ask,
}

impl DeliveryRefKind {
    /// Canonical lowercase label stored in `delivery_log.ref_kind`. The only inlined
    /// SQL "literals" for ref_kind are derived from this (compile-time constants,
    /// never user input).
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryRefKind::Message => "message",
            DeliveryRefKind::Notify => "notify",
            DeliveryRefKind::Ask => "ask",
        }
    }

    /// Parse a stored ref_kind back into the enum; an unknown value is a hard error
    /// at the store mapper (never a panic, never silently coerced). Currently a
    /// drift-guard/round-trip surface (the store binds the `.as_str()` verbatim and
    /// never reads the column back into an enum), so flagged like the Tier-2 methods.
    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "message" => Ok(DeliveryRefKind::Message),
            "notify" => Ok(DeliveryRefKind::Notify),
            "ask" => Ok(DeliveryRefKind::Ask),
            other => Err(format!("unknown delivery ref kind '{other}'")),
        }
    }
}

/// One stage in a delivery's transport trace, mapping weave's daemon-free reality
/// (inject + hook-drain; no websocket/broker) onto a repowire-style stage vocabulary:
/// `Queued` (persisted, awaiting nudge/drain) → `Injected` / `InjectFailed` /
/// `NotInjectable` (the caller-side live-nudge outcome) → `Drained` (consumed in a
/// recipient turn). Stored as TEXT (`delivery_log.stage`); validated through this
/// enum. Pure value type — DAG layer `model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStage {
    Queued,
    Injected,
    InjectFailed,
    NotInjectable,
    Drained,
}

impl DeliveryStage {
    /// Canonical label stored in `delivery_log.stage` (compile-time constant).
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStage::Queued => "queued",
            DeliveryStage::Injected => "injected",
            DeliveryStage::InjectFailed => "inject_failed",
            DeliveryStage::NotInjectable => "not_injectable",
            DeliveryStage::Drained => "drained",
        }
    }

    /// Parse a stored stage back into the enum; unknown is a hard error. Drift-guard/
    /// round-trip surface (see `DeliveryRefKind::from_str`).
    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "queued" => Ok(DeliveryStage::Queued),
            "injected" => Ok(DeliveryStage::Injected),
            "inject_failed" => Ok(DeliveryStage::InjectFailed),
            "not_injectable" => Ok(DeliveryStage::NotInjectable),
            "drained" => Ok(DeliveryStage::Drained),
            other => Err(format!("unknown delivery stage '{other}'")),
        }
    }
}

/// Coarse pass/fail of a delivery stage. `Ok` = the stage completed as intended
/// (queued, injected, not-injectable-but-persisted, drained); `Fail` = the stage's
/// operation errored (an inject that returned `Err`). Stored as TEXT
/// (`delivery_log.outcome`); validated through this enum. Pure value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryOutcome {
    Ok,
    Fail,
}

impl DeliveryOutcome {
    /// Canonical label stored in `delivery_log.outcome` (compile-time constant).
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryOutcome::Ok => "ok",
            DeliveryOutcome::Fail => "fail",
        }
    }

    /// Parse a stored outcome back into the enum; unknown is a hard error. Drift-guard/
    /// round-trip surface (see `DeliveryRefKind::from_str`).
    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "ok" => Ok(DeliveryOutcome::Ok),
            "fail" => Ok(DeliveryOutcome::Fail),
            other => Err(format!("unknown delivery outcome '{other}'")),
        }
    }
}

/// One row of a delivery's transport trace, surfaced read-only by
/// `weave_delivery` / `weave delivery`. METADATA ONLY — it deliberately carries NO
/// body, subject, sig, or token; the trace records *that and whether* a delivery
/// happened, never its content (the secret-free design point). `ref_id` points at
/// the `messages` row for an operator who wants the body (already access-controlled
/// by the inbox). Pure data assembled by the store at read time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryTrace {
    pub id: i64,
    pub ref_id: i64,
    pub ref_kind: String,
    pub to_peer: String,
    pub stage: String,
    pub outcome: String,
    pub ts: i64,
}

// ──────────────────────────────────────────────────────────────────────────
// WL-016 — Scheduler / cron for messages
// ──────────────────────────────────────────────────────────────────────────

/// Hard upper bound on a cron expression string (chars). Expressions are echoed
/// into listings and stored per-row; an unbounded one is a token/RAM/DoS hazard.
pub const MAX_CRON_EXPR_LEN: usize = 64;

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
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleKind::OneShot => "one_shot",
            ScheduleKind::Recurring => "recurring",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "one_shot" => Ok(ScheduleKind::OneShot),
            "recurring" => Ok(ScheduleKind::Recurring),
            other => Err(format!("unknown schedule kind '{other}'")),
        }
    }
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

/// Validate a cron expression before storage. Accepts presets (`@hourly`, `@daily`,
/// `@weekly`, `@monthly`) and a restricted 5-field subset (`min hour day month dow`).
/// Rejects empty, over-length, control-character-bearing, or unsupported forms.
pub fn cron_valid(expr: &str) -> bool {
    if expr.is_empty() || expr.len() > MAX_CRON_EXPR_LEN {
        return false;
    }
    if expr.bytes().any(|b| b.is_ascii_control()) {
        return false;
    }
    match expr {
        "@hourly" | "@daily" | "@weekly" | "@monthly" => return true,
        _ => {}
    }
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    for (i, part) in parts.iter().enumerate() {
        let (min, max) = match i {
            0 => (0, 59),
            1 => (0, 23),
            2 => (1, 31),
            3 => (1, 12),
            4 => (0, 6),
            _ => unreachable!(),
        };
        if !field_valid(part, min, max) {
            return false;
        }
    }
    true
}

fn field_valid(part: &str, min: i64, max: i64) -> bool {
    if part == "*" {
        return true;
    }
    if let Some((a, b)) = part.split_once('-') {
        let Ok(a) = a.parse::<i64>() else {
            return false;
        };
        let Ok(b) = b.parse::<i64>() else {
            return false;
        };
        return a >= min && b <= max && a <= b;
    }
    let Ok(n) = part.parse::<i64>() else {
        return false;
    };
    n >= min && n <= max
}

/// Internal representation of one cron field after parsing.
#[derive(Debug, Clone)]
enum CronField {
    Star,
    Exact(i64),
    Range(i64, i64),
}

fn parse_field(part: &str) -> Option<CronField> {
    if part == "*" {
        return Some(CronField::Star);
    }
    if let Some((a, b)) = part.split_once('-') {
        let a = a.parse::<i64>().ok()?;
        let b = b.parse::<i64>().ok()?;
        return Some(CronField::Range(a, b));
    }
    let n = part.parse::<i64>().ok()?;
    Some(CronField::Exact(n))
}

fn field_matches(cf: &CronField, value: i64) -> bool {
    match cf {
        CronField::Star => true,
        CronField::Exact(n) => *n == value,
        CronField::Range(a, b) => value >= *a && value <= *b,
    }
}

/// Parse a preset or 5-field expression into five [`CronField`]s.
fn parse_cron(expr: &str) -> Option<[CronField; 5]> {
    let s = match expr {
        "@hourly" => "0 * * * *",
        "@daily" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        _ => expr,
    };
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    Some([
        parse_field(parts[0])?,
        parse_field(parts[1])?,
        parse_field(parts[2])?,
        parse_field(parts[3])?,
        parse_field(parts[4])?,
    ])
}

/// Returns the NEXT UNIX timestamp **strictly after** `after` for the given
/// expression, or `None` if no occurrence falls within 366 days of `after`.
///
/// Supports presets (`@hourly`, `@daily`, `@weekly`, `@monthly`) and a
/// restricted 5-field cron (`min hour day month dow`) with `*` and `-` ranges.
/// Scans forward in 60-second increments — deterministic, dependency-free, and
/// fast enough for the coarse granularity weave needs.
pub fn next_occurrence(cron_expr: &str, after: i64) -> Option<i64> {
    let fields = parse_cron(cron_expr)?;
    // Start from the next whole minute strictly after `after`.
    let mut ts = after - after.rem_euclid(60) + 60;
    let max_ts = after + 366 * 86_400;

    while ts <= max_ts {
        let secs = ts.rem_euclid(86_400);
        let minute = (secs / 60) % 60;
        let hour = secs / 3_600;
        let days = ts.div_euclid(86_400);
        let (_y, month, day) = civil_from_days(days);
        let dow = (days + 4).rem_euclid(7); // 1970-01-01 was Thursday = 4

        if field_matches(&fields[0], minute)
            && field_matches(&fields[1], hour)
            && field_matches(&fields[2], day as i64)
            && field_matches(&fields[3], month as i64)
            && field_matches(&fields[4], dow)
        {
            return Some(ts);
        }
        ts += 60;
    }
    None
}

/// A cached LLM-generated summary for a message thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub root_id: i64,
    pub text: String,
    pub model: String,
    pub created_ts: i64,
    pub refreshed_ts: i64,
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

    /// Every `DeliveryStage` variant round-trips through `as_str`/`from_str`; an
    /// unknown label is a clean error (the exhaustiveness lock for the trace
    /// vocabulary — adding a variant without updating both maps fails this test).
    #[test]
    fn delivery_stage_str_roundtrips() {
        for s in [
            DeliveryStage::Queued,
            DeliveryStage::Injected,
            DeliveryStage::InjectFailed,
            DeliveryStage::NotInjectable,
            DeliveryStage::Drained,
        ] {
            assert_eq!(DeliveryStage::from_str(s.as_str()), Ok(s));
        }
        assert!(DeliveryStage::from_str("bogus").is_err());
        assert!(DeliveryStage::from_str("").is_err());
    }

    /// `DeliveryOutcome` round-trips; unknown is a clean error.
    #[test]
    fn delivery_outcome_str_roundtrips() {
        for s in [DeliveryOutcome::Ok, DeliveryOutcome::Fail] {
            assert_eq!(DeliveryOutcome::from_str(s.as_str()), Ok(s));
        }
        assert!(DeliveryOutcome::from_str("bogus").is_err());
    }

    /// `DeliveryRefKind` round-trips; unknown is a clean error.
    #[test]
    fn delivery_ref_kind_str_roundtrips() {
        for s in [
            DeliveryRefKind::Message,
            DeliveryRefKind::Notify,
            DeliveryRefKind::Ask,
        ] {
            assert_eq!(DeliveryRefKind::from_str(s.as_str()), Ok(s));
        }
        assert!(DeliveryRefKind::from_str("bogus").is_err());
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

    #[test]
    fn ask_many_id_validation() {
        let id = new_ask_many_id(42);
        assert!(id.starts_with("askm_42_"));
        assert!(ask_many_id_valid(&id));
        assert!(!ask_many_id_valid("")); // empty
        assert!(!ask_many_id_valid("ask_1_2")); // a plain child id is NOT a parent
        assert!(!ask_many_id_valid("askm 1")); // space
        assert!(!ask_many_id_valid("askm;rm")); // shell metachar
        assert!(!ask_many_id_valid(&format!(
            "askm_{}",
            "x".repeat(MAX_ASK_MANY_ID_LEN)
        ))); // oversized
    }

    #[test]
    fn new_ask_many_id_is_unique_per_mint() {
        let a = new_ask_many_id(1);
        let b = new_ask_many_id(1);
        assert_ne!(a, b);
    }

    /// `classify_ask_many` is the pure aggregate classifier the proptest targets.
    #[test]
    fn classify_ask_many_states() {
        // No pending children ⇒ complete regardless of age/threshold.
        assert_eq!(
            classify_ask_many(3, 0, 1, Some(100), Some(10)),
            AskManyState::Complete
        );
        // Pending children, no age threshold ⇒ never auto-partial (daemon-free).
        assert_eq!(
            classify_ask_many(3, 2, 0, Some(9999), None),
            AskManyState::Pending
        );
        // Pending children, threshold set but not yet elapsed ⇒ pending.
        assert_eq!(
            classify_ask_many(3, 1, 0, Some(5), Some(10)),
            AskManyState::Pending
        );
        // Pending children, threshold set and elapsed ⇒ partial.
        assert_eq!(
            classify_ask_many(3, 1, 0, Some(15), Some(10)),
            AskManyState::Partial
        );
        // A zero/negative threshold never flips to partial.
        assert_eq!(
            classify_ask_many(3, 1, 0, Some(15), Some(0)),
            AskManyState::Pending
        );
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

    // ---- P4: PeerRole + circle validators ----

    #[test]
    fn peer_role_round_trips_and_unknown_is_err() {
        for r in [PeerRole::Peer, PeerRole::Orchestrator] {
            assert_eq!(PeerRole::from_str(r.as_str()), Ok(r));
        }
        // A legacy empty value coalesces to the default `Peer`.
        assert_eq!(PeerRole::from_str(""), Ok(PeerRole::Peer));
        // Any other unknown value is a hard error (never silently coerced).
        assert!(PeerRole::from_str("admin").is_err());
        assert!(PeerRole::from_str("Orchestrator").is_err());
        assert_eq!(PeerRole::default(), PeerRole::Peer);
    }

    #[test]
    fn circle_valid_accepts_good_rejects_bad() {
        assert!(circle_valid("default"));
        assert!(circle_valid("team-a"));
        assert!(circle_valid("ops_1"));
        assert!(circle_valid(&"x".repeat(MAX_CIRCLE_LEN)));
        // empty / oversized / metachar are rejected.
        assert!(!circle_valid(""));
        assert!(!circle_valid(&"x".repeat(MAX_CIRCLE_LEN + 1)));
        for bad in [
            "a b", "a/b", "a;b", "a$b", "a`b", "a\nb", "a'b", "a\"b", "a|b",
        ] {
            assert!(!circle_valid(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn idempotency_key_valid_bounds() {
        assert!(idempotency_key_valid("uuid-123"));
        assert!(idempotency_key_valid(&"x".repeat(MAX_IDEMPOTENCY_KEY_LEN)));
        assert!(!idempotency_key_valid(""));
        assert!(!idempotency_key_valid(
            &"x".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1)
        ));
        assert!(!idempotency_key_valid("has\ncontrol"));
        assert!(!idempotency_key_valid("has\0nul"));
    }

    #[test]
    fn trace_id_valid_bounds() {
        assert!(trace_id_valid("trace-abc"));
        assert!(trace_id_valid(&"x".repeat(MAX_TRACE_ID_LEN)));
        assert!(!trace_id_valid(""));
        assert!(!trace_id_valid(&"x".repeat(MAX_TRACE_ID_LEN + 1)));
        assert!(!trace_id_valid("has\ncontrol"));
        assert!(!trace_id_valid("has\0nul"));
    }

    #[test]
    fn circle_or_default_maps_empty_to_default() {
        assert_eq!(circle_or_default(""), DEFAULT_CIRCLE);
        assert_eq!(circle_or_default("team"), "team");
        assert_eq!(DEFAULT_CIRCLE, "default");
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

        /// ASK-MANY AGGREGATE TOTALITY: for ANY child mix, `classify_ask_many` never
        /// panics and obeys its contract — `Complete` iff `pending == 0`; a `Partial`
        /// verdict only ever appears when a positive age threshold has elapsed and a
        /// child is still pending; otherwise `Pending`. The caller-side totality
        /// invariant (`answered + acked + pending + failed == total`) is honored by the
        /// generator so the property mirrors the store's read-time rollup.
        #[test]
        fn classify_ask_many_is_total(
            answered in 0i64..50,
            acked in 0i64..50,
            pending in 0i64..50,
            failed in 0i64..50,
            age in proptest::option::of(0i64..10_000),
            thr in proptest::option::of(0i64..10_000),
        ) {
            let total = answered + acked + pending + failed;
            let st = classify_ask_many(total, pending, failed, age, thr);
            // Totality holds by construction.
            prop_assert_eq!(answered + acked + pending + failed, total);
            // Complete iff no child pending.
            prop_assert_eq!(st == AskManyState::Complete, pending == 0);
            // Partial requires a positive elapsed threshold AND a pending child.
            if st == AskManyState::Partial {
                prop_assert!(pending > 0);
                let a = age.unwrap();
                let t = thr.unwrap();
                prop_assert!(t > 0 && a >= t);
            }
        }

        /// JOB STATE-MACHINE TOTALITY: for EVERY ordered pair over the full 11-state
        /// set, `can_transition` never panics, never moves OUT of a terminal state
        /// (only the idempotent self-noop), and ALWAYS admits cancel/expire as an
        /// interrupt from any non-terminal state. Deterministic. The pure invariant
        /// the store consults before every job UPDATE.
        #[test]
        fn job_transition_is_total(a in 0u8..11, b in 0u8..11) {
            let from = job_state_of(a);
            let to = job_state_of(b);
            let ok = from.can_transition(to);
            prop_assert_eq!(ok, from.can_transition(to)); // determinism
            if from.is_terminal() {
                prop_assert_eq!(ok, from == to); // terminal frozen
            } else {
                if to == JobState::Cancelled || to == JobState::Expired {
                    prop_assert!(ok); // interrupt always allowed
                }
                if to == JobState::Queued {
                    prop_assert_eq!(ok, from == JobState::Queued); // no resurrection
                }
            }
        }

        /// JOB TERMINAL IS ABSORBING: walking an arbitrary index sequence and only
        /// taking legal edges, once a job reaches ANY terminal state no further
        /// state-changing edge is legal (the daemon-free "done stays done" contract).
        #[test]
        fn job_lifecycle_terminal_is_absorbing(steps in proptest::collection::vec(0u8..11, 0..32)) {
            let mut cur = JobState::Queued;
            for &s in &steps {
                let next = job_state_of(s);
                if cur.is_terminal() {
                    prop_assert_eq!(cur.can_transition(next), cur == next);
                } else if cur.can_transition(next) {
                    cur = next;
                }
            }
        }

        /// ATTEMPT-ID MONOTONIC FENCING: across ANY number of (re)claims, every minted
        /// attempt id is valid and DISTINCT from every prior mint — so only the LATEST
        /// token can match the row and every earlier token is fenced out as stale.
        #[test]
        fn attempt_ids_are_unique_and_valid(n in 1usize..32) {
            use std::collections::HashSet;
            let mut seen: HashSet<String> = HashSet::new();
            for _ in 0..n {
                let id = new_attempt_id(42);
                prop_assert!(attempt_id_valid(&id));
                prop_assert!(id.starts_with("att_"));
                prop_assert!(seen.insert(id), "attempt ids must be unique per mint");
            }
        }

        /// JOB-ID VALIDITY TOTALITY: `job_id_valid` never panics on arbitrary input and
        /// its verdict matches the contract (non-empty, ≤ cap, `job_` prefix, ASCII
        /// `[A-Za-z0-9_]`); metachar/oversized/empty/wrong-prefix ids are always rejected.
        #[test]
        fn job_id_valid_is_total(s in ".*") {
            let got = job_id_valid(&s);
            let expect = !s.is_empty()
                && s.len() <= MAX_JOB_ID_LEN
                && s.starts_with("job_")
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            prop_assert_eq!(got, expect);
            if s.bytes().any(|b| matches!(b, b';' | b'|' | b'&' | b'$' | b'`' | b' ' | b'\n' | b'\'' | b'"')) {
                prop_assert!(!got);
            }
        }

        /// CIRCLE-VALIDITY TOTALITY (P4): `circle_valid` never panics on arbitrary
        /// input and its verdict matches the contract (non-empty, ≤ cap, ASCII
        /// `[A-Za-z0-9_-]`); a metachar/oversized/empty circle is always rejected —
        /// the `ask_id_valid_is_total`/`job_id_valid_is_total` precedent.
        #[test]
        fn circle_valid_is_total(s in ".*") {
            let got = circle_valid(&s);
            let expect = !s.is_empty()
                && s.len() <= MAX_CIRCLE_LEN
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            prop_assert_eq!(got, expect);
            if s.bytes().any(|b| matches!(b, b';' | b'|' | b'&' | b'$' | b'`' | b' ' | b'\n' | b'\'' | b'"' | b'/')) {
                prop_assert!(!got);
            }
        }

        /// EXPIRE-DESCRIPTION TOTALITY (P5): for ANY `(now, description_ts)` —
        /// including i64 extremes and negatives — `expire_description` never panics
        /// (saturating arithmetic) and matches its exact contract: the description is
        /// blanked IFF it was non-empty AND anchored (`ts > 0`) AND
        /// `now.saturating_sub(ts) >= DESCRIPTION_TTL_SECS`; otherwise it is left
        /// untouched. A never-anchored (`ts <= 0`) or empty description is never
        /// expired. Idempotent: a second call is a no-op.
        #[test]
        fn expire_description_is_total(
            now in any::<i64>(),
            ts in any::<i64>(),
            desc in proptest::option::of("[ -~]{1,40}"),
        ) {
            let mut p = prop_peer(desc.as_deref().unwrap_or(""), ts);
            let was_empty = p.description.is_empty();
            let should_expire = !was_empty
                && ts > 0
                && now.saturating_sub(ts) >= DESCRIPTION_TTL_SECS;
            let before = p.description.clone();
            expire_description(&mut p, now);
            if should_expire {
                prop_assert_eq!(&p.description, "", "an expired description must blank");
            } else {
                prop_assert_eq!(&p.description, &before, "a live/unanchored description is untouched");
            }
            // Idempotent: a second pass changes nothing.
            let after = p.description.clone();
            expire_description(&mut p, now);
            prop_assert_eq!(&p.description, &after, "expiry is idempotent");
        }

        /// TURN-STATE ROUND-TRIP TOTALITY (P5): every enum value's `as_str` parses
        /// back to itself, and `from_str` never panics on arbitrary input — an
        /// unknown value is always an `Err` (never a silent coercion), while the
        /// empty string and "unknown" coalesce to `Unknown`.
        #[test]
        fn turn_state_from_str_is_total(s in ".*") {
            // from_str never panics; the result matches the contract.
            let got = TurnState::from_str(&s);
            let known = matches!(
                s.as_str(),
                "" | "unknown" | "pending_first_turn" | "working" | "awaiting_input" | "idle"
            );
            prop_assert_eq!(got.is_ok(), known);
            // Every enum value round-trips through as_str.
            for st in [
                TurnState::Unknown,
                TurnState::PendingFirstTurn,
                TurnState::Working,
                TurnState::AwaitingInput,
                TurnState::Idle,
            ] {
                prop_assert_eq!(TurnState::from_str(st.as_str()).unwrap(), st);
            }
        }
    }

    /// Build a minimal Peer carrying just the description fields the `expire_description`
    /// proptest exercises (other fields are inert for that pure helper).
    fn prop_peer(desc: &str, ts: i64) -> Peer {
        Peer {
            name: "p".to_string(),
            mux: "tmux".to_string(),
            target: "%1".to_string(),
            socket: String::new(),
            cwd: None,
            last_seen: 0,
            pid: None,
            host: String::new(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: DEFAULT_CIRCLE.to_string(),
            role: PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: desc.to_string(),
            description_ts: ts,
            birth_cert: None,
            contact_policy: "open".to_string(),
        }
    }

    /// Map a small index onto a `JobState` so proptest can range over the full set.
    fn job_state_of(i: u8) -> JobState {
        match i % 11 {
            0 => JobState::Queued,
            1 => JobState::Dispatching,
            2 => JobState::Delivered,
            3 => JobState::Running,
            4 => JobState::AwaitingInput,
            5 => JobState::Completed,
            6 => JobState::Failed,
            7 => JobState::Cancelled,
            8 => JobState::Blocked,
            9 => JobState::Expired,
            _ => JobState::Unavailable,
        }
    }

    /// Every JobState label round-trips `from_str(as_str())` (the drift-guard for the
    /// only inlined SQL state literals), and an unknown label is a clean error.
    #[test]
    fn job_state_str_roundtrips() {
        use JobState::*;
        for s in [
            Queued,
            Dispatching,
            Delivered,
            Running,
            AwaitingInput,
            Completed,
            Failed,
            Cancelled,
            Blocked,
            Expired,
            Unavailable,
        ] {
            assert_eq!(JobState::from_str(s.as_str()), Ok(s));
        }
        assert!(JobState::from_str("bogus").is_err());
    }

    /// The job lifecycle machine: no edge OUT of a terminal state (only the
    /// idempotent self-noop), cancel/expire reachable from EVERY non-terminal state,
    /// and `can_transition` is total (never panics) over the whole 11×11 product.
    #[test]
    fn job_state_machine_totality() {
        use JobState::*;
        let all = [
            Queued,
            Dispatching,
            Delivered,
            Running,
            AwaitingInput,
            Completed,
            Failed,
            Cancelled,
            Blocked,
            Expired,
            Unavailable,
        ];
        for &from in &all {
            for &to in &all {
                let ok = from.can_transition(to);
                if from.is_terminal() {
                    // Terminal is frozen: only the same-state self-noop is allowed.
                    assert_eq!(ok, from == to, "terminal {from:?} -> {to:?}");
                } else {
                    // Cancel/expire interrupt every non-terminal state.
                    if to == Cancelled || to == Expired {
                        assert!(ok, "interrupt {from:?} -> {to:?} must be allowed");
                    }
                    // No transition ever lands on a state that then escapes terminality.
                    if ok && to.is_terminal() {
                        assert!(!to.can_transition(from) || to == from || from.is_terminal());
                    }
                }
            }
        }
    }

    /// A minted job id is always accepted by `job_id_valid`; an attempt id never is
    /// (and vice-versa) — the prefixes keep the two id spaces disjoint. Hostile /
    /// oversized / metachar values are rejected.
    #[test]
    fn job_and_attempt_id_validation() {
        let jid = new_job_id(7);
        assert!(jid.starts_with("job_7_"));
        assert!(job_id_valid(&jid));
        assert!(!attempt_id_valid(&jid)); // a job id is not an attempt id

        let aid = new_attempt_id(7);
        assert!(aid.starts_with("att_7_"));
        assert!(attempt_id_valid(&aid));
        assert!(!job_id_valid(&aid)); // an attempt id is not a job id

        assert!(!job_id_valid("")); // empty
        assert!(!job_id_valid("ask_1_2")); // wrong prefix
        assert!(!job_id_valid("job 1")); // space
        assert!(!job_id_valid("job;rm")); // shell metachar
        assert!(!job_id_valid(&format!(
            "job_{}",
            "x".repeat(MAX_JOB_ID_LEN)
        ))); // oversized
        assert!(!attempt_id_valid("att;rm")); // shell metachar
    }

    /// Distinct mints never collide for the same seed (the shared monotonic nonce
    /// widens the opaque tail) — this is what makes a re-claim fence out a stale token.
    #[test]
    fn job_ids_are_unique_per_mint() {
        assert_ne!(new_job_id(1), new_job_id(1));
        assert_ne!(new_attempt_id(1), new_attempt_id(1));
    }

    // ---- WL-016: cron evaluator ----

    #[test]
    fn schedule_kind_roundtrip() {
        assert_eq!(
            ScheduleKind::from_str("one_shot"),
            Ok(ScheduleKind::OneShot)
        );
        assert_eq!(
            ScheduleKind::from_str("recurring"),
            Ok(ScheduleKind::Recurring)
        );
        assert!(ScheduleKind::from_str("bogus").is_err());
        assert_eq!(ScheduleKind::OneShot.as_str(), "one_shot");
        assert_eq!(ScheduleKind::Recurring.as_str(), "recurring");
    }

    #[test]
    fn cron_valid_presets() {
        assert!(cron_valid("@hourly"));
        assert!(cron_valid("@daily"));
        assert!(cron_valid("@weekly"));
        assert!(cron_valid("@monthly"));
    }

    #[test]
    fn cron_valid_5field() {
        assert!(cron_valid("0 9 * * 1-5"));
        assert!(cron_valid("* * * * *"));
        assert!(cron_valid("30 12 15 * *"));
    }

    #[test]
    fn cron_valid_rejections() {
        assert!(!cron_valid(""));
        assert!(!cron_valid("* * * *")); // 4 fields
        assert!(!cron_valid("* * * * * *")); // 6 fields
        assert!(!cron_valid("60 * * * *")); // minute out of range
        assert!(!cron_valid("0 24 * * *")); // hour out of range
        assert!(!cron_valid("0 0 32 * *")); // day out of range
        assert!(!cron_valid("0 0 * 13 *")); // month out of range
        assert!(!cron_valid("0 0 * * 7")); // dow out of range
        assert!(!cron_valid(&"x".repeat(MAX_CRON_EXPR_LEN + 1)));
        assert!(!cron_valid("0\t0 * * *")); // control char
    }

    /// Helper: build a UNIX timestamp from civil fields (UTC).
    fn ts(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
        // Simple conversion for test data only; not the production path.
        // Parse via chrono if available in tests, otherwise approximate.
        // weave deliberately has no date crate, so we use a naive day-count
        // approximation for the test fixture.  This is test-only and only
        // needs to be consistent with civil_from_days.
        //
        // We'll use the reverse of civil_from_days: count days from 1970-01-01.
        // This is a test helper, not production code.
        let mut days = 0i64;
        for year in 1970..y {
            days += if is_leap_year(year) { 366 } else { 365 };
        }
        for month in 1..m {
            days += days_in_month(y, month);
        }
        days += (d as i64) - 1;
        days * 86_400 + hh * 3_600 + mm * 60
    }

    fn is_leap_year(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
    }

    fn days_in_month(y: i64, m: u32) -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap_year(y) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        }
    }

    #[test]
    fn next_occurrence_presets() {
        let midnight = ts(2024, 1, 1, 0, 0);
        // @daily from midnight → next day midnight
        assert_eq!(
            next_occurrence("@daily", midnight),
            Some(ts(2024, 1, 2, 0, 0))
        );
        // @hourly from 09:00 → 10:00
        assert_eq!(
            next_occurrence("@hourly", ts(2024, 1, 1, 9, 0)),
            Some(ts(2024, 1, 1, 10, 0))
        );
        // @weekly from Monday → next Sunday
        assert_eq!(
            next_occurrence("@weekly", ts(2024, 1, 1, 0, 0)),
            Some(ts(2024, 1, 7, 0, 0))
        );
        // @monthly from Jan 1 → Feb 1
        assert_eq!(
            next_occurrence("@monthly", ts(2024, 1, 1, 0, 0)),
            Some(ts(2024, 2, 1, 0, 0))
        );
    }

    #[test]
    fn next_occurrence_cron_ranges() {
        // 0 9 * * 1-5  => next weekday 09:00
        let fri_noon = ts(2024, 1, 5, 12, 0); // Fri 12:00
        assert_eq!(
            next_occurrence("0 9 * * 1-5", fri_noon),
            Some(ts(2024, 1, 8, 9, 0))
        ); // Mon

        let mon_0800 = ts(2024, 1, 8, 8, 0); // Mon 08:00
        assert_eq!(
            next_occurrence("0 9 * * 1-5", mon_0800),
            Some(ts(2024, 1, 8, 9, 0))
        ); // same Mon
    }

    #[test]
    fn next_occurrence_no_past() {
        // A missed daily schedule advances to the NEXT future day.
        let jan3 = ts(2024, 1, 3, 0, 0);
        assert_eq!(next_occurrence("@daily", jan3), Some(ts(2024, 1, 4, 0, 0)));
    }

    #[test]
    fn next_occurrence_rejects_bad_expr() {
        assert_eq!(next_occurrence("garbage", 0), None);
        assert_eq!(next_occurrence("* * * *", 0), None);
    }

    // ---- WL-020: review queue ----

    #[test]
    fn review_item_state_roundtrip() {
        assert_eq!(ReviewItemState::Open.as_str(), "open");
        assert_eq!(ReviewItemState::Merged.as_str(), "merged");
        assert_eq!(ReviewItemState::Closed.as_str(), "closed");
        assert_eq!(ReviewItemState::from_str("open"), Ok(ReviewItemState::Open));
        assert_eq!(
            ReviewItemState::from_str("merged"),
            Ok(ReviewItemState::Merged)
        );
        assert_eq!(
            ReviewItemState::from_str("closed"),
            Ok(ReviewItemState::Closed)
        );
        assert!(ReviewItemState::from_str("bogus").is_err());
    }

    #[test]
    fn review_queue_filter_roundtrip() {
        assert_eq!(ReviewQueueFilter::All.as_str(), "all");
        assert_eq!(
            ReviewQueueFilter::from_str("all"),
            Ok(ReviewQueueFilter::All)
        );
        assert_eq!(
            ReviewQueueFilter::from_str("pending"),
            Ok(ReviewQueueFilter::Pending)
        );
        assert_eq!(
            ReviewQueueFilter::from_str("reviewed"),
            Ok(ReviewQueueFilter::Reviewed)
        );
        assert!(ReviewQueueFilter::from_str("bogus").is_err());
    }

    #[test]
    fn pr_url_valid_accepts_github_rejects_others() {
        assert!(pr_url_valid("https://github.com/owner/repo/pull/1"));
        assert!(pr_url_valid("https://github.com/owner/repo/pull/123"));
        assert!(!pr_url_valid("https://example.com/pull/1"));
        assert!(!pr_url_valid("not-a-url"));
        assert!(!pr_url_valid(""));
    }

    #[test]
    fn review_id_valid_shape() {
        assert!(review_id_valid("review_1_123"));
        assert!(!review_id_valid("bad_1_123"));
        assert!(!review_id_valid(""));
    }

    #[test]
    fn new_review_id_is_unique() {
        assert_ne!(new_review_id(1), new_review_id(1));
    }

    // ---- WL-021: permission status ----

    #[test]
    fn permission_status_pending_open_within_timeout() {
        let ask = Ask {
            id: "ask_1_1".to_string(),
            question_msg_id: 1,
            answer_msg_id: None,
            asker: "a".to_string(),
            askee: "b".to_string(),
            subject: None,
            state: AskState::Open,
            kind: AskKind::ToolPermission,
            options: None,
            reply_to: None,
            close_note: None,
            opened_ts: 1000,
            updated_ts: 1000,
            closed_ts: None,
            parent_id: None,
        };
        assert_eq!(
            permission_status(&ask, None, 1200, 300),
            PermissionStatus::Pending
        );
    }

    #[test]
    fn permission_status_timeout_open_expired() {
        let ask = Ask {
            id: "ask_1_1".to_string(),
            question_msg_id: 1,
            answer_msg_id: None,
            asker: "a".to_string(),
            askee: "b".to_string(),
            subject: None,
            state: AskState::Open,
            kind: AskKind::ToolPermission,
            options: None,
            reply_to: None,
            close_note: None,
            opened_ts: 1000,
            updated_ts: 1000,
            closed_ts: None,
            parent_id: None,
        };
        assert_eq!(
            permission_status(&ask, None, 2000, 300),
            PermissionStatus::Timeout
        );
    }

    #[test]
    fn permission_status_approved_on_approve_body() {
        let ask = Ask {
            id: "ask_1_1".to_string(),
            question_msg_id: 1,
            answer_msg_id: Some(2),
            asker: "a".to_string(),
            askee: "b".to_string(),
            subject: None,
            state: AskState::Answered,
            kind: AskKind::ToolPermission,
            options: None,
            reply_to: None,
            close_note: None,
            opened_ts: 1000,
            updated_ts: 1200,
            closed_ts: None,
            parent_id: None,
        };
        assert_eq!(
            permission_status(&ask, Some("approve"), 1200, 300),
            PermissionStatus::Approved
        );
        assert_eq!(
            permission_status(&ask, Some("Approve"), 1200, 300),
            PermissionStatus::Approved
        );
    }

    #[test]
    fn permission_status_denied_on_non_approve() {
        let ask = Ask {
            id: "ask_1_1".to_string(),
            question_msg_id: 1,
            answer_msg_id: Some(2),
            asker: "a".to_string(),
            askee: "b".to_string(),
            subject: None,
            state: AskState::Answered,
            kind: AskKind::ToolPermission,
            options: None,
            reply_to: None,
            close_note: None,
            opened_ts: 1000,
            updated_ts: 1200,
            closed_ts: None,
            parent_id: None,
        };
        assert_eq!(
            permission_status(&ask, Some("deny"), 1200, 300),
            PermissionStatus::Denied
        );
        assert_eq!(
            permission_status(&ask, Some("no"), 1200, 300),
            PermissionStatus::Denied
        );
    }

    #[test]
    fn lease_resource_valid_accepts_good_rejects_bad() {
        assert!(lease_resource_valid("crates/foo/src/lib.rs"));
        assert!(lease_resource_valid("migrations/"));
        assert!(!lease_resource_valid(""));
        assert!(!lease_resource_valid("has\0null"));
        assert!(!lease_resource_valid("has\nnewline"));
        let oversize = "a".repeat(MAX_LEASE_RESOURCE_LEN + 1);
        assert!(!lease_resource_valid(&oversize));
    }

    #[test]
    fn lease_ttl_valid_bounds() {
        assert!(lease_ttl_valid(1));
        assert!(lease_ttl_valid(3600));
        assert!(lease_ttl_valid(MAX_LEASE_TTL_SECS));
        assert!(!lease_ttl_valid(0));
        assert!(!lease_ttl_valid(-1));
        assert!(!lease_ttl_valid(MAX_LEASE_TTL_SECS + 1));
    }

    #[test]
    fn ttl_valid_bounds() {
        assert!(ttl_valid(1));
        assert!(ttl_valid(3600));
        assert!(ttl_valid(MAX_MSG_TTL_SECS));
        assert!(!ttl_valid(0));
        assert!(!ttl_valid(-1));
        assert!(!ttl_valid(MAX_MSG_TTL_SECS + 1));
    }

    #[test]
    fn expiry_from_ttl_adds_and_saturates() {
        assert_eq!(expiry_from_ttl(1000, 60), 1060);
        assert_eq!(expiry_from_ttl(0, MAX_MSG_TTL_SECS), MAX_MSG_TTL_SECS);
        // i64::MAX base must saturate, not wrap/panic.
        assert_eq!(expiry_from_ttl(i64::MAX, 1), i64::MAX);
    }

    #[test]
    fn lease_path_normalize_collapses_and_strips() {
        assert_eq!(lease_path_normalize("/foo/bar/"), "foo/bar");
        assert_eq!(lease_path_normalize("//foo//bar//"), "foo/bar");
        assert_eq!(lease_path_normalize("foo/./bar"), "foo/bar");
        assert_eq!(lease_path_normalize("foo/bar/../baz"), "foo/baz");
        assert_eq!(lease_path_normalize("foo"), "foo");
        assert_eq!(lease_path_normalize(""), "");
    }

    #[test]
    fn lease_path_conflicts_detects_exact_and_ancestor() {
        assert!(lease_path_conflicts("foo/bar", "foo/bar"));
        assert!(lease_path_conflicts("foo/bar", "foo/bar/baz"));
        assert!(lease_path_conflicts("foo/bar/baz", "foo/bar"));
        assert!(!lease_path_conflicts("foo/bar", "foo/baz"));
        assert!(!lease_path_conflicts("foo/bar", "foo/barbie"));
        assert!(!lease_path_conflicts("foo", "foobar"));
    }
}
