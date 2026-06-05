//! Persistent message + peer store.
//!
//! [`Store`] is the backend-agnostic interface. [`SqliteStore`] (rusqlite, bundled)
//! is the default; a feature-gated libSQL/Turso backend implements the same trait
//! for cross-machine sync (see `store_libsql.rs`). The on-disk SQLite format is
//! libSQL-compatible, so the file is portable between backends.

use crate::config::StoreSource;
use crate::model::{now, Intent, Message, Peer};
use anyhow::Result;

// Re-export the libsql backend's federation aggregators under `store::` so the
// `main`/`mcp` consumers can call `store::federated_peers` / `federated_sessions`
// regardless of which (mutually-exclusive) backend is compiled in. The sqlite
// backend defines these free functions inline below.
#[cfg(feature = "libsql")]
pub use crate::store_libsql::{
    federated_peers, federated_sessions, federation_status, pull_from_store,
};

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
    /// Messages addressed to `me` (direct or broadcast) with `id > since_id` and
    /// `sender != me`, oldest-first, capped at `limit`. Lets `weave watch` page
    /// strictly forward from the last id it saw without dropping backlog (unlike
    /// `inbox`, which is unread-scoped and newest-first). Read-only: never marks
    /// anything read.
    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>>;
    fn sessions(&self) -> Result<Vec<SessionInfo>>;
    fn total_messages(&self) -> Result<i64>;
    fn clear_inbox(&self, me: &str) -> Result<usize>;
    fn clear_all(&self) -> Result<i64>;
    /// Delete messages (and their read-markers) older than `older_than_secs`.
    /// Returns how many messages were removed. Retention / disk-bound guard.
    fn gc(&self, older_than_secs: i64) -> Result<i64>;
    /// Register (upsert) a peer carrying every field, including the registering
    /// process's `pid` and `host` for real process-liveness. This is the full
    /// primitive each backend implements; the 6-arg [`Store::register_peer`]
    /// forwards here with `pid=None, host=""` so legacy call sites that do not
    /// know a PID/host keep working unchanged.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<()>;

    /// Register (upsert) a peer without PID/host liveness info or git tags.
    /// Additive backward-compatible wrapper over [`Store::register_peer_full`]:
    /// forwards with `pid=None, host=""` (== liveness unknown ⇒ presence falls
    /// back to the TTL recency guess) and empty git tags. Keeps existing 5-arg
    /// call sites/tests compiling.
    ///
    /// `allow(dead_code)`: weave is a binary crate, so a `pub` trait method with
    /// only test callers is otherwise flagged unused. This is intentional
    /// backward-compat surface (exercised by the store unit tests), not dead code.
    #[allow(dead_code)]
    fn register_peer(
        &self,
        name: &str,
        mux: &str,
        target: &str,
        socket: &str,
        cwd: Option<&str>,
    ) -> Result<()> {
        self.register_peer_full(name, mux, target, socket, cwd, None, "", "", "", "")
    }
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

    /// Tier-2: append a cross-store delivery **intent** to THIS store's own
    /// `outbox`, returning its new local intent id. Owner-only-writes: this is the
    /// sender writing its OWN store; it never touches the recipient's store. The
    /// recipient pulls it read-only and commits it locally. `sig` is reserved for
    /// signed identity (2d) and is `""` in 2a/2b.
    ///
    /// `allow(dead_code)`: weave is a binary crate, so a `pub` trait method whose
    /// only callers are tests / a not-yet-wired CLI arm is otherwise flagged
    /// unused. This is intentional Tier-2 surface, exercised by the store unit
    /// tests, not dead code.
    #[allow(dead_code)]
    fn enqueue_intent(
        &self,
        to: &str,
        to_host: &str,
        from: &str,
        subject: Option<&str>,
        body: &str,
        sig: &str,
    ) -> Result<i64>;

    /// Tier-2: read intents from THIS store's `outbox` addressed to `for_recipient`
    /// with `id > since_id`, oldest-first (ascending id), capped at `limit`. A
    /// read-only SELECT — used on the read-only foreign handle by
    /// [`pull_from_store`] to scan a source for messages addressed to the puller.
    #[allow(dead_code)]
    fn list_outbox(&self, for_recipient: &str, since_id: i64, limit: i64) -> Result<Vec<Intent>>;

    /// Tier-2: list ALL pending intents in THIS store's `outbox` (any recipient),
    /// oldest-first, capped at `limit`. Backs the `weave outbox` self-inspector.
    #[allow(dead_code)]
    fn outbox_all(&self, limit: i64) -> Result<Vec<Intent>>;

    /// Tier-2 receiver-side dedup cursor: the highest source-outbox intent id this
    /// store has already committed from `source` (canonical path label). `0` when
    /// nothing has been pulled from that source yet. Read from the LOCAL store.
    #[allow(dead_code)]
    fn pull_cursor_get(&self, source: &str) -> Result<i64>;

    /// Advance (upsert) this store's per-source pull cursor to `last_id`. Written
    /// to the LOCAL store only, after committing the corresponding intent.
    #[allow(dead_code)]
    fn pull_cursor_set(&self, source: &str, last_id: i64) -> Result<()>;

    /// Tier-2 (2d): register (upsert) a peer/session's hex-encoded ed25519 public
    /// key in the `keys` table, used to VERIFY signed intents claiming to be from
    /// that identity. The table is plain data (present in every build); only the
    /// SIGN/VERIFY crypto is behind the `sign` feature, so a `sign`-built receiver
    /// can read a key registered by any build. Bound `params!`.
    #[allow(dead_code)]
    fn register_key(&self, identity: &str, pubkey: &str) -> Result<()>;

    /// Fetch the registered public key for `identity` (hex), or `None` if unknown.
    #[allow(dead_code)]
    fn get_key(&self, identity: &str) -> Result<Option<String>>;

    /// List all registered `(identity, pubkey)` pairs, ordered by identity. Backs
    /// `weave key list`.
    #[allow(dead_code)]
    fn list_keys(&self) -> Result<Vec<(String, String)>>;
}

/// Where a federated row came from. `Local` is this session's own store;
/// `Foreign` carries a short display label (the configured store's basename or
/// path) so a listing can tell local from federated entries. Backend-agnostic
/// data (no I/O), shared by both store backends and the `main`/`mcp` consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Local,
    Foreign(String),
}

impl Origin {
    /// Short label for display/JSON: `"local"` or the foreign store's label.
    pub fn label(&self) -> &str {
        match self {
            Origin::Local => "local",
            Origin::Foreign(s) => s,
        }
    }

    /// True for any non-local (federated) origin.
    pub fn is_foreign(&self) -> bool {
        matches!(self, Origin::Foreign(_))
    }
}

/// A peer tagged with the store it was read from (Tier-1 federation). Keeps
/// [`Peer`] itself unchanged while carrying provenance for display + dedup.
#[derive(Debug, Clone)]
pub struct PeerView {
    pub peer: Peer,
    pub origin: Origin,
}

/// A session row tagged with its origin store (Tier-1 federation).
/// `(name, unread, last_activity)` mirrors [`SessionInfo`]; foreign rows are kept
/// distinct (origin-tagged) rather than arithmetic-merged, because Tier 1 cannot
/// deliver a cross-store inbox so summing unread across stores would mislead.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub name: String,
    pub unread: i64,
    pub last_activity: i64,
    pub origin: Origin,
}

/// Derive a short, display-friendly label for a foreign store from its path: the
/// file's basename (e.g. `messages.db`), falling back to the full path string when
/// there is no usable file name. Pure; used to tag `Foreign` origins.
pub fn store_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Merge + dedup federated peer views on the key `(name, host)`, applying the
/// Tier-1 tie-break so the same logical session seen via two stores collapses to
/// one entry. Pure (no I/O) so it is exhaustively unit-testable.
///
/// Tie-break on collision, in order:
/// 1. **alive beats not-alive** — `is_alive(peer)` true wins;
/// 2. else **most-recently-seen wins** — higher `last_seen`;
/// 3. else **prefer the local store** over a foreign one (local is authoritative
///    for a session that registered here);
/// 4. else a stable order by the origin label, so the merge is reproducible.
///
/// The surviving entries are returned sorted by peer name (then origin label) for
/// a deterministic listing.
pub fn merge_peer_views(views: Vec<PeerView>) -> Vec<PeerView> {
    // (name, host) -> chosen view. Iterate, keeping the winner per key.
    let mut chosen: Vec<PeerView> = Vec::new();
    for v in views {
        match chosen
            .iter_mut()
            .find(|c| c.peer.name == v.peer.name && c.peer.host == v.peer.host)
        {
            Some(existing) => {
                if peer_view_beats(&v, existing) {
                    *existing = v;
                }
            }
            None => chosen.push(v),
        }
    }
    chosen.sort_by(|a, b| {
        a.peer
            .name
            .cmp(&b.peer.name)
            .then_with(|| a.origin.label().cmp(b.origin.label()))
    });
    chosen
}

/// True if `candidate` should replace `current` for the same `(name, host)` key,
/// per the [`merge_peer_views`] tie-break.
fn peer_view_beats(candidate: &PeerView, current: &PeerView) -> bool {
    let (ca, cu) = (is_alive(&candidate.peer), is_alive(&current.peer));
    if ca != cu {
        return ca; // alive beats not-alive
    }
    if candidate.peer.last_seen != current.peer.last_seen {
        return candidate.peer.last_seen > current.peer.last_seen; // newer wins
    }
    // Same aliveness + recency: prefer local over foreign.
    let (c_local, u_local) = (
        matches!(candidate.origin, Origin::Local),
        matches!(current.origin, Origin::Local),
    );
    if c_local != u_local {
        return c_local;
    }
    // Final deterministic tie-break by origin label (candidate replaces only if
    // strictly "smaller", so the result is order-independent).
    candidate.origin.label() < current.origin.label()
}

/// Merge federated session views keyed on `name` (sessions have no host). On
/// collision keep `max(last_activity)` and **do not sum unread** across stores —
/// a message in another store is not in this session's local inbox, so summing
/// would imply a unified inbox Tier 1 cannot deliver. The local row's unread is
/// authoritative; if only foreign rows exist for a name, the most-recent foreign
/// unread is kept but origin-tagged so the UI can signal it is not local.
/// Pure (no I/O); returned sorted by name then origin label.
pub fn merge_session_views(views: Vec<SessionView>) -> Vec<SessionView> {
    let mut chosen: Vec<SessionView> = Vec::new();
    for v in views {
        match chosen.iter_mut().find(|c| c.name == v.name) {
            Some(existing) => {
                // Activity is the max across stores.
                existing.last_activity = existing.last_activity.max(v.last_activity);
                // Unread: a local row is authoritative. Otherwise keep the entry
                // we already had unless this one is local (which wins) — never sum.
                let existing_local = matches!(existing.origin, Origin::Local);
                let v_local = matches!(v.origin, Origin::Local);
                if v_local && !existing_local {
                    existing.unread = v.unread;
                    existing.origin = v.origin;
                }
            }
            None => chosen.push(v),
        }
    }
    chosen.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.origin.label().cmp(b.origin.label()))
    });
    chosen
}

/// True if `last_seen` is within the online (TTL) window relative to `now_ts`.
/// Pure: the single source of truth for the recency comparison, parameterized on
/// the clock so it is deterministic in tests (used by [`liveness_for`]).
pub fn is_online_at(last_seen: i64, now_ts: i64) -> bool {
    now_ts.saturating_sub(last_seen) <= ONLINE_TTL_SECS
}

/// True if `last_seen` is within the online window relative to the real now.
/// Thin wrapper over [`is_online_at`] reading the wall clock. Test-only since
/// `is_alive` now delegates to [`liveness_for`] (which uses [`is_online_at`]);
/// kept for the recency-boundary assertions in both backends' test suites.
#[cfg(test)]
pub fn is_online(last_seen: i64) -> bool {
    is_online_at(last_seen, now())
}

/// True if a process with PID `pid` currently exists on THIS machine.
///
/// Crate-free and OS-conditional:
/// - **Linux**: checks `/proc/<pid>` existence (no dependency).
/// - **other targets**: degrades to "assume alive" so non-Linux callers fall
///   back to the TTL recency guess (we add no `libc`/`nix` dependency just to
///   probe a PID; `is_alive` only ever consults this on the local host).
#[cfg(target_os = "linux")]
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// See the Linux variant. On non-Linux targets we have no dependency-free PID
/// probe, so report "alive" and let presence rely on the TTL window.
#[cfg(not(target_os = "linux"))]
pub fn pid_alive(_pid: i64) -> bool {
    true
}

/// Host-aware liveness verdict for a peer (the "A2 — fail-open by host" rule).
///
/// This is the single, PURE classifier behind presence. It takes `this_host`
/// and `now_ts` as parameters so it is exhaustively testable with fixed values
/// (no real hostname/clock); the only I/O is the same-host PID probe, which is
/// gated to the local arm and never runs for a remote host.
///
/// Three regimes:
/// - [`Liveness::AliveLocal`] — `peer.host == this_host` AND within the TTL
///   window AND (the PID probe passes OR the PID is unknown). A same-host
///   null-pid recent row is alive-by-TTL (still local).
/// - [`Liveness::AliveRemote`] — `peer.host != this_host` (INCLUDING an empty
///   host, since `this_host` is never empty) AND within the TTL window. A
///   remote peer is NEVER pid-probed — we cannot probe a process on another
///   machine, so we fail OPEN to the recency guess (the Turso/shared-DB case).
/// - [`Liveness::Stale`] — offline (past the TTL window), OR a same-host row
///   whose known PID is dead. A dead-but-recent local process reads stale.
///
/// The pid-confirmed-vs-TTL-presumed nuance is surfaced only in the human reason
/// string ([`Liveness::reason`]), not as a fourth variant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Liveness {
    /// Same host, online, and pid-confirmed (or null-pid TTL fallback).
    AliveLocal,
    /// Remote host (incl. empty host), online by TTL only — never pid-probed.
    AliveRemote,
    /// Offline (past TTL) or a same-host known-dead pid.
    Stale,
}

impl Liveness {
    /// Stable snake_case token for machine-readable output (`scan --json`).
    pub fn token(self) -> &'static str {
        match self {
            Liveness::AliveLocal => "alive_local",
            Liveness::AliveRemote => "alive_remote",
            Liveness::Stale => "stale",
        }
    }
}

/// Host-aware liveness classifier — see [`Liveness`]. Pure except for the
/// same-host PID probe (which is gated to the `AliveLocal` arm).
pub fn liveness_for(peer: &Peer, this_host: &str, now_ts: i64) -> Liveness {
    // Recency guard first: anything past the TTL window is stale regardless of host.
    if !is_online_at(peer.last_seen, now_ts) {
        return Liveness::Stale;
    }
    if peer.host == this_host {
        // Same host: the PID is authoritative when known; a dead local pid is
        // stale even though it is recent. A null pid falls back to the TTL window.
        match peer.pid {
            Some(pid) if !pid_alive(pid) => Liveness::Stale,
            _ => Liveness::AliveLocal,
        }
    } else {
        // Remote host (incl. empty host): fail OPEN to the TTL verdict — NEVER
        // probe a pid we cannot resolve on this machine.
        Liveness::AliveRemote
    }
}

/// Real liveness verdict for a peer as a bool — the recency + host-aware rule.
/// Thin wrapper over [`liveness_for`] reading the real `this_host()`/`now()` so
/// every existing bool call site sees byte-identical results.
///
/// Rules (unchanged from the previous direct implementation):
/// - Always require `is_online(last_seen)` (the recency guard).
/// - If `host == this_host()` AND a PID is known, additionally require
///   [`pid_alive`] — a dead-but-recent local process reads offline.
/// - **Fail OPEN otherwise** (`host != this_host()`, or PID unknown): we cannot
///   probe a remote PID (Turso/shared-DB case) or an unknown one, so we fall
///   back to the TTL recency guess. A remote/legacy peer must NOT read dead.
pub fn is_alive(peer: &Peer) -> bool {
    !matches!(
        liveness_for(peer, &crate::config::this_host(), now()),
        Liveness::Stale
    )
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

/// Hard upper bound on an identity label (sender/recipient/peer name) in chars.
/// Identities are echoed into other agents' prompts and used as map keys, so an
/// unbounded one is a token/RAM/UI hazard. 128 chars is generous for any real
/// session name.
pub const MAX_IDENT: usize = 128;

/// Validate an identity label (sender, recipient, or peer name) before it is
/// stored. Rejects empty, over-length (> [`MAX_IDENT`] chars), or
/// control-character-bearing values. `label` names the field for the error
/// message. Shared by both backends so CLI/MCP/hook are all covered at the store
/// layer. Additive: only previously-invalid input is now refused.
pub fn check_ident(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must not be empty.");
    }
    let chars = value.chars().count();
    if chars > MAX_IDENT {
        anyhow::bail!("{label} is too long ({chars} chars; max {MAX_IDENT}).");
    }
    if value.chars().any(|c| c.is_control()) {
        anyhow::bail!("{label} must not contain control characters.");
    }
    Ok(())
}

/// Validate an optional cross-store host hint before it is stored on an intent.
/// Empty is allowed (== unspecified). A non-empty host is bounded to
/// [`crate::config::MAX_HOST_LEN`] chars and must be control-character-free, the
/// same discipline as a derived host label, so a hostile foreign `to_host` cannot
/// smuggle an unbounded or control-bearing value through the outbox.
pub fn check_host(host: &str) -> Result<()> {
    if host.is_empty() {
        return Ok(());
    }
    let chars = host.chars().count();
    if chars > crate::config::MAX_HOST_LEN {
        anyhow::bail!(
            "to_host is too long ({chars} chars; max {}).",
            crate::config::MAX_HOST_LEN
        );
    }
    if host.chars().any(|c| c.is_control()) {
        anyhow::bail!("to_host must not contain control characters.");
    }
    Ok(())
}

/// Hard upper bounds on the descriptive session tags (`repo`, `branch`,
/// `worktree_id`), in chars. These are captured from the cwd / git at
/// registration and echoed into other agents' listings, so an unbounded one is a
/// token/RAM/UI hazard exactly like an identity. 128 chars matches
/// [`MAX_IDENT`] / [`crate::config::MAX_HOST_LEN`] and is generous for any real
/// repo name, git ref, or `.git/worktrees/<name>` component.
pub const MAX_REPO_LEN: usize = 128;
pub const MAX_BRANCH_LEN: usize = 128;
pub const MAX_WORKTREE_LEN: usize = 128;

/// Sanitize a descriptive tag (repo/branch/worktree_id) for storage. UNLIKE
/// [`check_ident`] these are NOT injection targets and NOT identities, so the
/// rule is **lossy-but-total**: strip control characters and truncate to `max`
/// chars on a UTF-8 boundary, never hard-fail. This is the `config::this_host`
/// idiom (`trim → drop control → take(max)`), applied at the single store seam so
/// every capture path (CLI register/attach, hook session, MCP attach/scan) is
/// bounded identically. `max` is the per-tag cap (`MAX_REPO_LEN` etc.). An
/// all-control / empty input collapses to `""` (== "unknown tag"), which the
/// display layer renders as `-`.
pub fn sanitize_tag(value: &str, max: usize) -> String {
    // Drop control chars FIRST, then trim: trimming before the control-strip lets a
    // trailing control char (e.g. `"x \u{7f}"`) shield a space that re-surfaces once
    // the control is removed, so a single pass yields `"x "` while a second yields
    // `"x"` — breaking idempotency. Truncation to `max` can likewise re-expose a
    // trailing space at the cap boundary, so trim the end again afterward. The
    // result is total, control-free, ≤ `max` chars on a UTF-8 boundary, and a fixed
    // point of `sanitize_tag` (`sanitize(sanitize(x)) == sanitize(x)`).
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    let mut out: String = cleaned.trim().chars().take(max).collect();
    out.truncate(out.trim_end().len());
    out
}

/// Tier-2 DoS bound: the maximum number of intents [`pull_from_store`] commits
/// from a single source in one drain. A flood in a source's outbox cannot make
/// one receiver drain unbounded — the per-source high-water cursor means the rest
/// arrive on subsequent drains (never lost). Mirrors the per-call ceilings
/// [`MAX_SESSIONS`] / `MAX_PEER_DBS`.
pub const MAX_PULL_PER_DRAIN: i64 = 256;

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
);
CREATE TABLE IF NOT EXISTS outbox (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    to_peer   TEXT NOT NULL,
    to_host   TEXT NOT NULL DEFAULT '',
    from_peer TEXT NOT NULL,
    subject   TEXT,
    body      TEXT NOT NULL,
    sig       TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS pull_cursor (
    source  TEXT PRIMARY KEY,
    last_id INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS keys (
    identity TEXT PRIMARY KEY,
    pubkey   TEXT NOT NULL
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
        socket: r.get(3)?,
        cwd: r.get(4)?,
        last_seen: r.get(5)?,
        pid: r.get(6)?,
        host: r.get(7)?,
        repo: r.get(8)?,
        branch: r.get(9)?,
        worktree_id: r.get(10)?,
    })
}

/// Map an `outbox` row to an [`Intent`]. Column order matches the explicit
/// projections used below: id, ts, to_peer, to_host, from_peer, subject, body, sig.
#[cfg(feature = "sqlite")]
fn row_to_intent(r: &Row) -> rusqlite::Result<Intent> {
    Ok(Intent {
        id: r.get(0)?,
        ts: r.get(1)?,
        to: r.get(2)?,
        to_host: r.get(3)?,
        from: r.get(4)?,
        subject: r.get(5)?,
        body: r.get(6)?,
        sig: r.get(7)?,
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
    // peers.socket — present on fresh DBs via SCHEMA, added here for DBs created
    // before kitty-socket persistence existed. Defaults to '' for every existing
    // row (== socket unknown), matching `Peer::socket`'s empty default.
    if !column_exists(conn, "peers", "socket")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN socket TEXT NOT NULL DEFAULT '';")?;
    }
    // peers.pid — present on fresh DBs via SCHEMA, added here for DBs created
    // before process-liveness existed. Nullable; defaults to NULL for every
    // existing row (== PID unknown ⇒ presence falls back to the TTL guess),
    // matching `Peer::pid`'s `None` default.
    if !column_exists(conn, "peers", "pid")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN pid INTEGER;")?;
    }
    // peers.host — present on fresh DBs via SCHEMA, added here for DBs created
    // before process-liveness existed. Defaults to '' for every existing row
    // (== host unknown ⇒ liveness fails open / TTL-only), matching `Peer::host`'s
    // empty default.
    if !column_exists(conn, "peers", "host")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN host TEXT NOT NULL DEFAULT '';")?;
    }
    // peers.repo / branch / worktree_id — present on fresh DBs via SCHEMA, added
    // here for DBs created before session-scan tagging existed. Each defaults to
    // '' for every existing row (== tag unknown), matching the empty `Peer`
    // defaults. The DDL identifiers are constant (no user data), the same
    // discipline as the socket/pid/host steps above.
    if !column_exists(conn, "peers", "repo")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN repo TEXT NOT NULL DEFAULT '';")?;
    }
    if !column_exists(conn, "peers", "branch")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN branch TEXT NOT NULL DEFAULT '';")?;
    }
    if !column_exists(conn, "peers", "worktree_id")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN worktree_id TEXT NOT NULL DEFAULT '';")?;
    }
    // Tier-2 tables: present on fresh DBs via SCHEMA, created here for DBs made
    // before cross-store delivery existed. `CREATE TABLE IF NOT EXISTS` is itself
    // idempotent, so this is a clean additive upgrade for a legacy store; the
    // `sig` column is reserved now so signed identity (2d) needs no further
    // outbox migration.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS outbox (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ts        INTEGER NOT NULL,
            to_peer   TEXT NOT NULL,
            to_host   TEXT NOT NULL DEFAULT '',
            from_peer TEXT NOT NULL,
            subject   TEXT,
            body      TEXT NOT NULL,
            sig       TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS pull_cursor (
            source  TEXT PRIMARY KEY,
            last_id INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS keys (
            identity TEXT PRIMARY KEY,
            pubkey   TEXT NOT NULL
        );",
    )?;
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

    /// Open an EXISTING store **read-only** for Tier-1 federation. The connection
    /// is opened with `SQLITE_OPEN_READ_ONLY` (no `CREATE`), so the SQLite engine
    /// itself rejects any write — the read-only guarantee is structural, not a
    /// convention. We deliberately DO NOT create the file, run `SCHEMA`, call
    /// `migrate()`, or `harden_permissions`: a foreign store we do not own must be
    /// read exactly as-is and never altered. A missing/locked/non-weave file
    /// surfaces here (or on first SELECT) as an error so the caller can skip it.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        use rusqlite::OpenFlags;
        // READ_ONLY rejects writes; NO_MUTEX matches our single-threaded use; we
        // intentionally omit CREATE so a missing file errors rather than being
        // created.
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    /// Unread messages for `me` (inherent helper; used by `sessions`).
    fn unread_count(&self, me: &str) -> Result<i64> {
        unread_count_conn(&self.conn, me)
    }
}

/// Diagnostic: of the configured `extra` read-only stores, how many open + list
/// cleanly vs. are skipped this run (missing/locked/non-weave). Used by `doctor`
/// to surface federation health without emitting the per-store stderr skip notes
/// twice. Read-only + best-effort, like the aggregators.
#[cfg(feature = "sqlite")]
pub fn federation_status(extra: &[StoreSource]) -> (usize, usize) {
    let mut ok = 0usize;
    let mut skipped = 0usize;
    for src in extra {
        match src {
            StoreSource::Local(path) => {
                match SqliteStore::open_readonly(path).and_then(|s| s.list_peers()) {
                    Ok(_) => ok += 1,
                    Err(_) => skipped += 1,
                }
            }
            // The default (sqlite) build cannot open a remote libsql URL — count it
            // as skipped (the loud per-source note is emitted by the aggregators).
            StoreSource::Remote { .. } => skipped += 1,
        }
    }
    (ok, skipped)
}

/// Loud, non-silent rejection of a REMOTE source on the default (sqlite) build:
/// remote libSQL/Turso sources require a `--features libsql` build. Prints the
/// URL **scheme + host only** (never the token, never the full URL/query) to
/// **stderr** and is counted as a skip by the caller. The command still succeeds on
/// any local sources.
#[cfg(feature = "sqlite")]
fn reject_remote_source(url: &str) {
    eprintln!(
        "[weave] skipping remote source '{}': remote libsql/Turso sources require \
         building weave with --features libsql",
        remote_scheme_host(url)
    );
}

/// Redacted display of a remote URL for diagnostics: scheme + host only, dropping
/// any path/query (which could carry secrets). Falls back to the scheme when no
/// host can be parsed. Never prints the auth token.
pub fn remote_scheme_host(url: &str) -> String {
    // Split off scheme://rest, then take up to the first '/', '?' or '#'.
    if let Some((scheme, rest)) = url.split_once("://") {
        let host: String = rest
            .chars()
            .take_while(|&c| c != '/' && c != '?' && c != '#')
            .collect();
        if host.is_empty() {
            format!("{scheme}://")
        } else {
            format!("{scheme}://{host}")
        }
    } else {
        // Not a recognizable URL; show only the scheme-ish prefix to avoid leaking.
        url.chars().take_while(|&c| c != '/').collect()
    }
}

/// Aggregate the local store's peers with those of each configured read-only
/// extra store (Tier-1 federation), origin-tagged and deduped on `(name, host)`.
///
/// Each foreign store is opened **read-only** via [`SqliteStore::open_readonly`]
/// (structurally incapable of writing it) and listed via the existing
/// `list_peers` SELECT. **Failure isolation:** an unreadable / locked / missing /
/// non-weave extra store is logged to **stderr** and skipped — it never breaks the
/// local listing. With `extra` empty this is exactly `local.list_peers()`
/// tagged `Local`, i.e. identical-to-today.
#[cfg(feature = "sqlite")]
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
        let path = match src {
            StoreSource::Local(p) => p,
            // Default sqlite build cannot open a remote URL: reject loudly + skip.
            StoreSource::Remote { url, .. } => {
                reject_remote_source(url);
                continue;
            }
        };
        let label = store_label(path);
        match SqliteStore::open_readonly(path).and_then(|s| s.list_peers()) {
            Ok(peers) => {
                for peer in peers {
                    views.push(PeerView {
                        peer,
                        origin: Origin::Foreign(label.clone()),
                    });
                }
            }
            Err(e) => {
                eprintln!("[weave] skipping federated store '{}': {e}", path.display());
            }
        }
    }
    Ok(merge_peer_views(views))
}

/// Aggregate the local store's sessions with those of each configured read-only
/// extra store (Tier-1 federation), origin-tagged and merged by name (keeping
/// `max(last_activity)`, never summing unread — see [`merge_session_views`]).
/// Same read-only open + per-store failure isolation as [`federated_peers`].
#[cfg(feature = "sqlite")]
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
        let path = match src {
            StoreSource::Local(p) => p,
            StoreSource::Remote { url, .. } => {
                reject_remote_source(url);
                continue;
            }
        };
        let label = store_label(path);
        match SqliteStore::open_readonly(path).and_then(|s| s.sessions()) {
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
                eprintln!("[weave] skipping federated store '{}': {e}", path.display());
            }
        }
    }
    Ok(merge_session_views(views))
}

/// A digest of one [`pull_from_store`] run, for the drain to log to stderr and to
/// drive the caller-side Tier-2 consent nudge (decision 5).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// Intents committed into the LOCAL inbox this run.
    pub committed: usize,
    /// Sources that were skipped (missing / locked / non-weave / no outbox).
    pub sources_skipped: usize,
    /// The `allow` sources that committed at least one intent this run, in
    /// first-seen order. The CALLER (main/mcp) uses these to gate the consent
    /// nudge per source (`Config::inject_allowed_from_source`) WITHOUT `store` ever
    /// depending on `inject` — the inject decision is made caller-side, keeping
    /// the `store → inject` edge from forming. These are the original (un-
    /// canonicalized) `allow` sources (local path OR remote URL) so the caller can
    /// match them against `allow_inject_from`.
    pub committed_sources: Vec<StoreSource>,
}

/// Tier-2 cross-store delivery (receiver side). For each `allow`-listed source
/// store, open it **read-only** (the ONLY foreign touch), read the intents
/// addressed to `me` since this store's per-source cursor, and **commit each into
/// the LOCAL store via the normal [`Store::send`]** — the receiver assigns its own
/// id/ts, anchoring ordering/dedup locally (owner-only-writes).
///
/// Structural owner-only-writes guarantee: the foreign store is opened ONLY via
/// [`SqliteStore::open_readonly`] (SQLite rejects any write), and EVERY write this
/// function performs — the committed inbox rows and the cursor advance — is to
/// `local`. The source file is never written, migrated, or created.
///
/// Authorization is receiver-side: a source NOT in `allow` is never opened, so it
/// can never deliver. Each committed intent's `from`/`to` is re-validated
/// (`check_ident`) — untrusted foreign data is bounded again at commit (defense in
/// depth) — and a failing intent is skipped (logged) rather than aborting the
/// batch. Idempotency: the per-source cursor is a strict high-water mark on the
/// source's outbox id, advanced after each commit, so a re-drain starts past
/// already-committed intents and never double-delivers.
///
/// Best-effort: an unreadable / locked / missing / non-weave / no-`outbox` source
/// is logged to **stderr** and skipped (the `federated_peers` failure-isolation
/// The signed-identity verification POLICY threaded into the pull/commit path (2d).
/// Carries the receiver's local trust domain: the tri-state strict override, the
/// trust set, and the revocation list. Constructed from `Config` at every pull call
/// site (`main`/`mcp`); it is the single input to the verification decision table.
///
/// Available in EVERY build (so the `pull_from_store`/`commit_pulled` signatures are
/// backend-identical), but only CONSULTED on the `sign` path — in a no-`sign` build
/// the fields are inert (the advisory model runs exactly as 2a–2c). The trust/revoked
/// lists are FULL-fingerprint or full-pubkey-hex strings (R3: matched against the full
/// SHA-256 digest, never a truncated display form).
#[derive(Clone, Debug, Default)]
// The fields are READ only on the `sign` pull path (trust/revocation/strict
// decision); in a no-`sign` build they are inert policy data carried through the
// backend-identical signature, so suppress the expected dead-field warning there.
#[cfg_attr(not(feature = "sign"), allow(dead_code))]
pub struct VerifyPolicy {
    /// `WEAVE_STRICT_VERIFY` / `Config::strict_verify` tri-state override:
    /// `Some(true)` force strict everywhere, `Some(false)` disable strict for the
    /// unsigned/unknown path (NEVER re-admits a revoked key's signed message, R1),
    /// `None` ⇒ the trust-set-aware default decides per sender.
    pub strict_override: Option<bool>,
    /// Trusted sender fingerprints / full pubkey hex. Non-empty ⇒ a trust set is
    /// "configured": a trusted sender is verified STRICTLY by default.
    pub trust: Vec<String>,
    /// Revoked fingerprints / full pubkey hex. A signature verifying against one of
    /// these is REJECTED unconditionally (R1).
    pub revoked: Vec<String>,
}

impl VerifyPolicy {
    /// The advisory (no-trust-set, no-override) policy: identical to today's
    /// `strict=false`. A test/back-compat constructor (production builds the struct
    /// from `Config` directly), so it is `cfg(test)`-scoped to stay warning-clean.
    #[cfg(test)]
    pub fn advisory() -> Self {
        Self::default()
    }

    /// A policy with ONLY the global strict override set (no trust set, nothing
    /// revoked) — the pre-trust-set behavior of the old bare `strict: bool` param.
    /// `strict(false)` == [`advisory`](Self::advisory) plus an explicit disable;
    /// `strict(true)` forces strict everywhere. Test/back-compat constructor, used
    /// only by the sign-gated decision-table tests (which live in the sqlite test
    /// module), so it is gated to exactly where it is referenced.
    #[cfg(all(test, feature = "sqlite", feature = "sign"))]
    pub fn strict(strict: bool) -> Self {
        Self {
            strict_override: Some(strict),
            ..Self::default()
        }
    }

    /// Is `pubkey_hex` (the sender's REGISTERED key) in the trust set? Matched
    /// against the FULL SHA-256 digest / full pubkey hex (R3). Always `false` in a
    /// no-`sign` build (no fingerprint to compute).
    #[cfg(feature = "sign")]
    fn is_trusted(&self, pubkey_hex: &str) -> bool {
        self.trust
            .iter()
            .any(|e| crate::sign::fingerprint_matches(e, pubkey_hex))
    }

    /// Is `pubkey_hex` (the sender's REGISTERED key) in the revocation list?
    #[cfg(feature = "sign")]
    fn is_revoked(&self, pubkey_hex: &str) -> bool {
        self.revoked
            .iter()
            .any(|e| crate::sign::fingerprint_matches(e, pubkey_hex))
    }

    /// Is a trust set CONFIGURED (non-empty)?
    #[cfg(feature = "sign")]
    fn trust_configured(&self) -> bool {
        !self.trust.is_empty()
    }
}

/// pattern) — it never breaks the local inbox drain. Per-source commits are bounded
/// by [`MAX_PULL_PER_DRAIN`] (the rest arrive on later drains, never lost).
///
/// `policy` (`VerifyPolicy`, 2d) controls the signed-identity decision: the tri-state
/// strict override, the trust set, and the revocation list. A trusted sender is
/// verified strictly; a revoked key's signed message is always rejected; a
/// tampered/forged signature is rejected regardless. `policy` is inert in a build
/// without the `sign` feature.
#[cfg(feature = "sqlite")]
pub fn pull_from_store(
    local: &dyn Store,
    me: &str,
    allow: &[StoreSource],
    policy: &VerifyPolicy,
) -> Result<Pulled> {
    let mut out = Pulled::default();
    for src in allow {
        let path = match src {
            StoreSource::Local(p) => p,
            // Default sqlite build cannot open a remote URL: reject loudly + skip.
            StoreSource::Remote { url, .. } => {
                reject_remote_source(url);
                out.sources_skipped += 1;
                continue;
            }
        };
        let source = canonical_source(path);
        let foreign = match SqliteStore::open_readonly(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[weave] skipping pull source '{}': {e}", path.display());
                out.sources_skipped += 1;
                continue;
            }
        };
        let since = local.pull_cursor_get(&source)?;
        let intents = match foreign.list_outbox(me, since, MAX_PULL_PER_DRAIN) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[weave] skipping pull source '{}': {e}", path.display());
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

/// Commit a batch of pulled intents (ascending id) into the LOCAL store and
/// advance the per-source cursor after each. Shared by both backends' free fns so
/// the dedup/validation/ordering rule is single-sourced.
///
/// Each intent is re-validated and committed via `local.send`; the cursor is set
/// to that intent's id immediately after, so a crash mid-batch resumes strictly
/// past the last committed intent (idempotent — never double-delivers). An intent
/// failing validation/commit is logged and skipped; the cursor still advances past
/// it so a poison row cannot wedge the source forever.
///
/// Signed identity (2d, `sign` feature): the per-intent decision is delegated to
/// [`verify_pulled_intent`] under the threaded [`VerifyPolicy`] (trust set,
/// revocation list, tri-state strict override). A tampered/forged signature is
/// ALWAYS rejected; a revoked key's signed message is always rejected; a trusted
/// sender is verified strictly; everything else follows the advisory model. In a
/// build without the `sign` feature, `policy` is inert and `sig` is ignored
/// (advisory model, exactly as 2a–2c). Verification reads only `local` (the
/// receiver's own key table); the source store is never written.
pub fn commit_pulled(
    local: &dyn Store,
    me: &str,
    source: &str,
    policy: &VerifyPolicy,
    intents: Vec<Intent>,
) -> Result<usize> {
    // `policy` is only consulted on the `sign` path; mark it used otherwise.
    #[cfg(not(feature = "sign"))]
    let _ = policy;
    let mut committed = 0usize;
    for intent in intents {
        // Defense in depth: re-validate untrusted foreign data at the commit seam
        // (the source's enqueue already capped it, but the receiver does not trust
        // the source). A bad intent is skipped, not fatal — but the cursor still
        // advances past it so it cannot wedge the source.
        let valid = check_ident("sender", &intent.from).is_ok()
            && check_ident("recipient", &intent.to).is_ok()
            && check_body(&intent.body).is_ok()
            && intent.to == me;
        // Signed identity (2d): gate the commit on signature verification when the
        // `sign` feature is built. A tampered sig is always rejected; an unsigned /
        // no-registered-key intent is dropped only under `strict_verify`. Without
        // the feature, `ok` is just structural validity (advisory model, as 2a–2c).
        #[cfg(feature = "sign")]
        let ok = valid && verify_pulled_intent(local, source, policy, &intent);
        #[cfg(not(feature = "sign"))]
        let ok = valid;
        if ok {
            match local.send(&intent.from, me, intent.subject.as_deref(), &intent.body) {
                Ok(_) => committed += 1,
                Err(e) => {
                    eprintln!(
                        "[weave] skipping intent #{} from source '{source}': {e}",
                        intent.id
                    );
                }
            }
        } else {
            eprintln!(
                "[weave] skipping malformed/misaddressed intent #{} from source '{source}'",
                intent.id
            );
        }
        // Advance the high-water cursor past this intent regardless of commit
        // outcome: a poison/misaddressed row must not block later intents.
        local.pull_cursor_set(source, intent.id)?;
    }
    Ok(committed)
}

/// Signed-identity commit gate (2d, `sign` feature). Implements the NEW
/// trust-set-aware decision table under the threaded [`VerifyPolicy`]. Reads only
/// `local` (the receiver's own `keys` table); never touches the source.
///
/// Two load-bearing rules hold in EVERY row:
///   1. A present-but-INVALID signature (tampered/forged/no-key-to-check) is ALWAYS
///      rejected, regardless of any strict toggle (preserved verbatim from the
///      original gate).
///   2. **R1 — absolute revocation:** a signature that VERIFIES against a REVOKED
///      key's fingerprint is REJECTED unconditionally, evaluated BEFORE the
///      `Some(false)` disable toggle can relax anything. The global-disable toggle
///      governs only the unsigned/unknown advisory path — it can NEVER re-admit a
///      revoked key's signed message.
///
/// Effective strictness for the UNSIGNED / no-key advisory path (R1 ordering):
/// ```text
/// if strict_override == Some(true)            => STRICT   (user forced)
/// else if strict_override == Some(false)      => ADVISORY (user disabled)
/// else if trust_configured && is_trusted(key) => STRICT   (NEW default)
/// else                                        => ADVISORY (current default)
/// ```
/// (Revocation for the unsigned/identity-claim case is folded into the override:
/// with `Some(false)` an unsigned message merely CLAIMING a revoked sender may relax
/// to advisory; but any actual signature verifying against the revoked pubkey is
/// rejected above, before the toggle is consulted.)
#[cfg(feature = "sign")]
fn verify_pulled_intent(
    local: &dyn Store,
    source: &str,
    policy: &VerifyPolicy,
    intent: &Intent,
) -> bool {
    // Look up the sender's REGISTERED key once: needed both to verify a signature and
    // to evaluate trust/revocation (which are keyed by the registered key's
    // fingerprint). A lookup error is a hard drop (cannot make a safe decision).
    let pubkey = match local.get_key(&intent.from) {
        Ok(pk) => pk,
        Err(e) => {
            eprintln!(
                "[weave] dropping intent #{} from source '{source}': key lookup failed: {e}",
                intent.id
            );
            return false;
        }
    };

    if !intent.sig.is_empty() {
        // ---- SIGNED PATH ----
        let pk = match &pubkey {
            Some(pk) => pk,
            // A signature we cannot check (no registered key): it is "present but
            // unverifiable". This is NOT a trusted/revoked sender (no fp to match),
            // so it falls to the advisory path's effective strictness below.
            None => return self_unsigned_or_unknown(source, policy, &pubkey, intent),
        };
        match crate::sign::verify_intent(pk, &intent.sig, &intent.from, &intent.to, &intent.body) {
            Ok(true) => {
                // A VALID signature. R1: if it verifies against a REVOKED key, reject
                // unconditionally — evaluated BEFORE any disable toggle.
                if policy.is_revoked(pk) {
                    eprintln!(
                        "[weave] REJECTING intent #{} from '{}' via source '{source}': \
                         signature verifies but the key is REVOKED",
                        intent.id, intent.from
                    );
                    return false;
                }
                true
            }
            Ok(false) => {
                // Present-but-invalid signature: ALWAYS rejected (spoof/tamper).
                eprintln!(
                    "[weave] REJECTING intent #{} from '{}' via source '{source}': signature \
                     verification failed (possible forgery)",
                    intent.id, intent.from
                );
                false
            }
            Err(e) => {
                eprintln!(
                    "[weave] dropping intent #{} from source '{source}': verify error: {e}",
                    intent.id
                );
                false
            }
        }
    } else {
        // ---- UNSIGNED PATH ----
        self_unsigned_or_unknown(source, policy, &pubkey, intent)
    }
}

/// The advisory-or-strict decision for an UNSIGNED intent, or a SIGNED intent whose
/// sender has no registered key to check it against. Computes effective strictness
/// per the R1 ordering and commits (advisory) or drops (strict). `pubkey` is the
/// sender's registered key (if any), used only to evaluate trust.
#[cfg(feature = "sign")]
fn self_unsigned_or_unknown(
    source: &str,
    policy: &VerifyPolicy,
    pubkey: &Option<String>,
    intent: &Intent,
) -> bool {
    let strict = match policy.strict_override {
        Some(true) => true,   // user forced strict everywhere
        Some(false) => false, // user disabled strict for the advisory path
        None => {
            // Trust-set-aware default: a TRUSTED sender (registered key in the trust
            // set) is verified strictly; everyone else stays advisory.
            policy.trust_configured() && pubkey.as_deref().is_some_and(|pk| policy.is_trusted(pk))
        }
    };
    if strict {
        if intent.sig.is_empty() {
            eprintln!(
                "[weave] dropping unsigned intent #{} from '{}' via source '{source}' \
                 (strict verification: trusted/forced sender must sign)",
                intent.id, intent.from
            );
        } else {
            eprintln!(
                "[weave] dropping signed intent #{} from '{}' via source '{source}': \
                 no registered key for sender (strict verification)",
                intent.id, intent.from
            );
        }
        return false;
    }
    true
}

/// Canonical per-source label for the `pull_cursor` key: the canonicalized path
/// string (falling back to the lossy path string when the file cannot be
/// canonicalized), so `./a.db` and its absolute form share one cursor — the same
/// canonicalization discipline `peer_db_paths` uses for dedup.
pub fn canonical_source(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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
        check_ident("sender", sender)?;
        check_ident("recipient", recipient)?;
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

    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>> {
        let limit = clamp_limit(limit);
        let sql = format!(
            "SELECT * FROM messages
             WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
            bc = BROADCAST_SQL
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![me, since_id, limit], row_to_message)?
            .collect::<rusqlite::Result<_>>()?;
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
        check_ident("peer name", name)?;
        // Descriptive git tags are bounded + control-free at this single store
        // seam (lossy-but-total), so every capture path is covered identically.
        let repo = sanitize_tag(repo, MAX_REPO_LEN);
        let branch = sanitize_tag(branch, MAX_BRANCH_LEN);
        let worktree_id = sanitize_tag(worktree_id, MAX_WORKTREE_LEN);
        self.conn.execute(
            "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(name) DO UPDATE SET mux=?2, target=?3, socket=?4, cwd=?5, last_seen=?6, pid=?7, host=?8, repo=?9, branch=?10, worktree_id=?11",
            params![name, mux, target, socket, cwd, now(), pid, host, repo, branch, worktree_id],
        )?;
        Ok(())
    }

    fn get_peer(&self, name: &str) -> Result<Option<Peer>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id FROM peers WHERE name=?1",
        )?;
        let mut it = stmt.query_map(params![name], row_to_peer)?;
        match it.next() {
            Some(p) => Ok(Some(p?)),
            None => Ok(None),
        }
    }

    fn list_peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id FROM peers ORDER BY name",
        )?;
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

    fn enqueue_intent(
        &self,
        to: &str,
        to_host: &str,
        from: &str,
        subject: Option<&str>,
        body: &str,
        sig: &str,
    ) -> Result<i64> {
        check_ident("recipient", to)?;
        check_ident("sender", from)?;
        check_host(to_host)?;
        check_body(body)?;
        self.conn.execute(
            "INSERT INTO outbox (ts, to_peer, to_host, from_peer, subject, body, sig)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![now(), to, to_host, from, subject, body, sig],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn list_outbox(&self, for_recipient: &str, since_id: i64, limit: i64) -> Result<Vec<Intent>> {
        let limit = clamp_limit(limit);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig FROM outbox
             WHERE to_peer = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![for_recipient, since_id, limit], row_to_intent)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn outbox_all(&self, limit: i64) -> Result<Vec<Intent>> {
        let limit = clamp_limit(limit);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig FROM outbox
             ORDER BY id ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], row_to_intent)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn pull_cursor_get(&self, source: &str) -> Result<i64> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT last_id FROM pull_cursor WHERE source = ?1",
                params![source],
                |r| r.get(0),
            )
            .ok();
        Ok(v.unwrap_or(0))
    }

    fn pull_cursor_set(&self, source: &str, last_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pull_cursor (source, last_id) VALUES (?1, ?2)
             ON CONFLICT(source) DO UPDATE SET last_id = ?2",
            params![source, last_id],
        )?;
        Ok(())
    }

    fn register_key(&self, identity: &str, pubkey: &str) -> Result<()> {
        check_ident("identity", identity)?;
        self.conn.execute(
            "INSERT INTO keys (identity, pubkey) VALUES (?1, ?2)
             ON CONFLICT(identity) DO UPDATE SET pubkey = ?2",
            params![identity, pubkey],
        )?;
        Ok(())
    }

    fn get_key(&self, identity: &str) -> Result<Option<String>> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT pubkey FROM keys WHERE identity = ?1",
                params![identity],
                |r| r.get(0),
            )
            .ok();
        Ok(v)
    }

    fn list_keys(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT identity, pubkey FROM keys ORDER BY identity")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

/// Pure dedup/tie-break tests for the federation merge helpers. Backend-agnostic
/// (the merge functions have no I/O), so this module runs under BOTH backends.
#[cfg(test)]
mod federation_tests {
    use super::*;

    fn peer(name: &str, host: &str, last_seen: i64, pid: Option<i64>) -> Peer {
        Peer {
            name: name.to_string(),
            mux: "tmux".to_string(),
            target: "%1".to_string(),
            socket: String::new(),
            cwd: None,
            last_seen,
            pid,
            host: host.to_string(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
        }
    }

    /// The same `(name, host)` seen via local + foreign collapses to ONE entry,
    /// and a fresh `last_seen` (both not pid-probed ⇒ both TTL-alive) makes the
    /// newer row win.
    #[test]
    fn merge_collapses_same_name_host_newer_wins() {
        let local = PeerView {
            peer: peer("prompt_hub", "boxA", now() - 100, None),
            origin: Origin::Local,
        };
        let foreign = PeerView {
            peer: peer("prompt_hub", "boxA", now() - 5, None),
            origin: Origin::Foreign("other.db".to_string()),
        };
        let merged = merge_peer_views(vec![local, foreign]);
        assert_eq!(merged.len(), 1, "same (name,host) collapses to one");
        assert_eq!(merged[0].peer.last_seen, now() - 5, "newer last_seen wins");
    }

    /// Different hosts are NOT collapsed: the same name on two machines is two
    /// distinct logical sessions.
    #[test]
    fn merge_keeps_distinct_hosts() {
        let a = PeerView {
            peer: peer("x", "boxA", now(), None),
            origin: Origin::Local,
        };
        let b = PeerView {
            peer: peer("x", "boxB", now(), None),
            origin: Origin::Foreign("o.db".to_string()),
        };
        let merged = merge_peer_views(vec![a, b]);
        assert_eq!(merged.len(), 2);
    }

    /// Alive beats not-alive regardless of recency: a stale-but-alive vs a
    /// recent-but-offline collision keeps the alive one. We build aliveness via the
    /// recency window (no pid ⇒ TTL-only): an online row beats an offline one even
    /// when the offline row is "newer" only relative to itself.
    #[test]
    fn merge_prefers_alive_over_offline() {
        // Online (recent) local row.
        let online = PeerView {
            peer: peer("p", "boxA", now(), None),
            origin: Origin::Local,
        };
        // Offline (stale) foreign row, but with a *higher* last_seen would still be
        // stale here; use a clearly stale value so is_alive == false.
        let offline = PeerView {
            peer: peer("p", "boxA", now() - ONLINE_TTL_SECS - 100, None),
            origin: Origin::Foreign("o.db".to_string()),
        };
        let merged = merge_peer_views(vec![offline, online]);
        assert_eq!(merged.len(), 1);
        assert!(is_alive(&merged[0].peer), "the alive row survives");
        assert_eq!(merged[0].origin, Origin::Local);
    }

    /// Merge tie-break lock through the `is_alive` wrapper for the new enum:
    /// a same-`(name, host)` collision where one row classifies `AliveRemote`
    /// (remote host, recent) and the other `Stale` (same remote host, past the
    /// TTL) must keep the ALIVE row — alive-beats-stale holds even though both
    /// rows are remote (so neither is pid-probed). This locks that `liveness_for`
    /// surfacing did not perturb `peer_view_beats` (which still keys off the
    /// `is_alive` bool).
    #[test]
    fn merge_alive_remote_beats_stale_remote_same_key() {
        // Both rows are on the SAME remote host so they collide on (name, host);
        // `now()` is the real clock here (the merge layer uses the bool wrapper),
        // but recent-vs-(now - TTL - 100) is unambiguously alive-vs-stale.
        let remote_host = "some-other-machine";
        let alive_remote = PeerView {
            peer: peer("svc", remote_host, now(), Some(999_999_999)),
            origin: Origin::Foreign("a.db".to_string()),
        };
        let stale_remote = PeerView {
            peer: peer(
                "svc",
                remote_host,
                now() - ONLINE_TTL_SECS - 100,
                Some(999_999_999),
            ),
            origin: Origin::Foreign("b.db".to_string()),
        };
        // Sanity: with this_host != remote_host these classify AliveRemote / Stale.
        assert_eq!(
            liveness_for(&alive_remote.peer, "this-host", now()),
            Liveness::AliveRemote
        );
        assert_eq!(
            liveness_for(&stale_remote.peer, "this-host", now()),
            Liveness::Stale
        );
        // Order-independent: the alive remote row survives both feed orders.
        let m1 = merge_peer_views(vec![alive_remote.clone(), stale_remote.clone()]);
        let m2 = merge_peer_views(vec![stale_remote, alive_remote]);
        assert_eq!(m1.len(), 1);
        assert_eq!(m2.len(), 1);
        assert!(is_alive(&m1[0].peer), "alive remote survives (order A)");
        assert!(is_alive(&m2[0].peer), "alive remote survives (order B)");
        assert_eq!(m1[0].origin, Origin::Foreign("a.db".to_string()));
        assert_eq!(m2[0].origin, Origin::Foreign("a.db".to_string()));
    }

    /// On equal aliveness AND equal recency, the LOCAL origin wins the tie.
    #[test]
    fn merge_local_wins_final_tie() {
        let ts = now();
        let local = PeerView {
            peer: peer("p", "boxA", ts, None),
            origin: Origin::Local,
        };
        let foreign = PeerView {
            peer: peer("p", "boxA", ts, None),
            origin: Origin::Foreign("o.db".to_string()),
        };
        // Order-independent: local must win whichever way they are fed in.
        let m1 = merge_peer_views(vec![local.clone(), foreign.clone()]);
        let m2 = merge_peer_views(vec![foreign, local]);
        assert_eq!(m1[0].origin, Origin::Local);
        assert_eq!(m2[0].origin, Origin::Local);
    }

    /// Result order is deterministic: sorted by peer name then origin label.
    #[test]
    fn merge_output_is_sorted_deterministically() {
        let views = vec![
            PeerView {
                peer: peer("zeta", "h", now(), None),
                origin: Origin::Local,
            },
            PeerView {
                peer: peer("alpha", "h", now(), None),
                origin: Origin::Foreign("o.db".to_string()),
            },
        ];
        let merged = merge_peer_views(views);
        let names: Vec<&str> = merged.iter().map(|v| v.peer.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    /// Federated sessions merge by name: keep `max(last_activity)` and DO NOT sum
    /// unread (a local row's unread is authoritative; foreign unread is never
    /// added to it).
    #[test]
    fn merge_sessions_max_activity_no_unread_sum() {
        let local = SessionView {
            name: "s".to_string(),
            unread: 3,
            last_activity: 100,
            origin: Origin::Local,
        };
        let foreign = SessionView {
            name: "s".to_string(),
            unread: 99,
            last_activity: 250,
            origin: Origin::Foreign("o.db".to_string()),
        };
        let merged = merge_session_views(vec![foreign, local]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].last_activity, 250, "max activity kept");
        assert_eq!(
            merged[0].unread, 3,
            "local unread authoritative, NOT summed"
        );
        assert_eq!(merged[0].origin, Origin::Local);
    }

    /// A session present ONLY in a foreign store is kept, origin-tagged foreign.
    #[test]
    fn merge_sessions_keeps_foreign_only() {
        let foreign = SessionView {
            name: "only-there".to_string(),
            unread: 2,
            last_activity: 10,
            origin: Origin::Foreign("o.db".to_string()),
        };
        let merged = merge_session_views(vec![foreign]);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].origin.is_foreign());
    }

    /// `store_label` derives the basename for a foreign store path.
    #[test]
    fn store_label_uses_basename() {
        assert_eq!(
            store_label(std::path::Path::new("/home/x/proj/messages.db")),
            "messages.db"
        );
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
        s.register_peer("envctl", "tmux", "%7", "/run/k.sock", Some("/w"))
            .unwrap();
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
        assert_eq!(after.socket, "/run/k.sock");
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
    fn check_ident_rejects_bad_and_accepts_good() {
        assert!(check_ident("sender", "desktop").is_ok());
        assert!(check_ident("sender", "").is_err(), "empty rejected");
        assert!(
            check_ident("sender", &"x".repeat(MAX_IDENT)).is_ok(),
            "exactly MAX_IDENT chars is allowed"
        );
        assert!(
            check_ident("sender", &"x".repeat(MAX_IDENT + 1)).is_err(),
            "over MAX_IDENT chars rejected"
        );
        assert!(
            check_ident("sender", "a\nb").is_err(),
            "control char rejected"
        );
        assert!(
            check_ident("sender", "a\tb").is_err(),
            "tab is a control char and rejected"
        );
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
        // A valid send still works (no regression).
        assert!(s.send("a", "b", None, "x").is_ok());
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

    /// `sanitize_tag` is lossy-but-total: it strips control chars, truncates to
    /// the cap on a UTF-8 char boundary, and never panics or hard-fails.
    #[test]
    fn sanitize_tag_strips_control_and_truncates_on_boundary() {
        // Control chars (newline, NUL, ESC) are dropped; surrounding ws trimmed.
        assert_eq!(
            sanitize_tag("  feat/x\n\0\u{1b}  ", MAX_BRANCH_LEN),
            "feat/x"
        );
        // Over-cap input truncates to exactly `max` CHARS (not bytes).
        let long = "ä".repeat(MAX_REPO_LEN + 50); // each 'ä' is 2 bytes, 1 char
        let out = sanitize_tag(&long, MAX_REPO_LEN);
        assert_eq!(out.chars().count(), MAX_REPO_LEN);
        assert!(out.is_char_boundary(out.len()));
        // All-control / empty collapses to "".
        assert_eq!(sanitize_tag("\n\t\0", MAX_WORKTREE_LEN), "");
        assert_eq!(sanitize_tag("", MAX_REPO_LEN), "");
        // Git-ref punctuation (/, ., -, _) is preserved verbatim (not control).
        assert_eq!(
            sanitize_tag("feature/foo-bar.v1_2", MAX_BRANCH_LEN),
            "feature/foo-bar.v1_2"
        );
    }

    proptest::proptest! {
        // Keep proptest from persisting a regression file in the source tree (the
        // e2e prop suite uses the same policy) and cap cases — this is pure + fast.
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 512,
            failure_persistence: None,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// `sanitize_tag` totality property: for ANY input string it never panics,
        /// the output is control-character-free, never exceeds the cap in CHARS,
        /// always lands on a UTF-8 boundary, and is IDEMPOTENT
        /// (`sanitize(sanitize(x)) == sanitize(x)`).
        #[test]
        fn prop_sanitize_tag_total_controlfree_capped_idempotent(s in ".*") {
            let cap = MAX_REPO_LEN; // 128
            let out = sanitize_tag(&s, cap);
            // Control-free.
            proptest::prop_assert!(
                !out.chars().any(|c| c.is_control()),
                "output must be control-free: {out:?}"
            );
            // Capped in chars (≤ 128) and on a valid boundary.
            proptest::prop_assert!(out.chars().count() <= cap);
            proptest::prop_assert!(out.is_char_boundary(out.len()));
            // Idempotent.
            proptest::prop_assert_eq!(sanitize_tag(&out, cap), out.clone());
        }

        /// `liveness_for` TOTALITY + DETERMINISM, with FIXED `this_host` + `now_ts`
        /// (never the real hostname/clock — the determinism mandate). For ANY
        /// `(host, this_host, last_seen, now_ts)`:
        /// - it never panics,
        /// - it is deterministic (two calls with the same inputs are equal),
        /// - the no-cross-host-probe guarantee: a row whose host differs from
        ///   `this_host` (incl. the empty host) is NEVER `Stale` *because of a pid*
        ///   — it is `Stale` IFF it is offline, and `AliveRemote` otherwise,
        ///   regardless of the (absurd) pid it carries.
        ///
        /// The pid is held to `None` or our OWN live pid so the property stays
        /// `/proc`-stable on the same-host arm (the only arm that probes). The
        /// same-host known-dead-pid regime is covered exhaustively by the unit
        /// matrix (`liveness_for_matrix_fixed_host_and_now`), not here, to keep
        /// this property independent of `/proc` contents for arbitrary pids.
        #[test]
        fn prop_liveness_for_total_deterministic_no_cross_host_probe(
            host in "[a-z0-9-]{0,20}",
            this_host in "[a-z0-9-]{1,20}",
            last_seen in proptest::prelude::any::<i64>(),
            now_ts in proptest::prelude::any::<i64>(),
            use_own_pid in proptest::prelude::any::<bool>(),
        ) {
            // pid is either absent or our OWN (live) pid — never an arbitrary pid,
            // so the same-host arm's /proc probe is deterministic regardless of the
            // machine the suite runs on.
            let pid = if use_own_pid {
                Some(std::process::id() as i64)
            } else {
                None
            };
            let p = Peer {
                name: "x".to_string(),
                mux: "tmux".to_string(),
                target: "%1".to_string(),
                socket: String::new(),
                cwd: None,
                last_seen,
                pid,
                host: host.clone(),
                repo: String::new(),
                branch: String::new(),
                worktree_id: String::new(),
            };

            // Determinism: two evaluations of the same inputs agree.
            let a = liveness_for(&p, &this_host, now_ts);
            let b = liveness_for(&p, &this_host, now_ts);
            proptest::prop_assert_eq!(a, b, "liveness_for must be deterministic");

            // Totality + the Stale characterization.
            let online = is_online_at(last_seen, now_ts);
            if !online {
                proptest::prop_assert_eq!(
                    a, Liveness::Stale,
                    "offline (past TTL) must be Stale regardless of host/pid"
                );
            } else if host == this_host {
                // Same host, online: our own live pid (or null) => AliveLocal.
                // (A dead pid would be Stale, but we never feed an arbitrary pid.)
                proptest::prop_assert_eq!(
                    a, Liveness::AliveLocal,
                    "same-host online with null/own-live pid => AliveLocal"
                );
            } else {
                // No-cross-host-probe: a remote/empty-host online row is NEVER
                // Stale by pid — it is AliveRemote, even with our own pid present.
                proptest::prop_assert_eq!(
                    a, Liveness::AliveRemote,
                    "remote/empty host online => AliveRemote (never pid-probed)"
                );
            }

            // Cross-cut: the Stale verdict implies (offline) OR (same-host).
            // A remote/empty-host online row can never be Stale.
            if a == Liveness::Stale {
                proptest::prop_assert!(
                    !online || host == this_host,
                    "Stale => offline OR same-host (never a remote-host pid probe)"
                );
            }
        }
    }

    /// The three git tags round-trip through `register_peer_full`, `get_peer`,
    /// and `list_peers`; an upsert overwrites them and a hostile control-bearing
    /// tag is stored sanitized (never rejected-fatal).
    #[test]
    fn git_tags_roundtrip_and_sanitize_through_upsert() {
        let s = mem();
        s.register_peer_full(
            "p",
            "tmux",
            "%1",
            "",
            Some("/w"),
            None,
            "h",
            "weave",
            "feat/x",
            "wt-1",
        )
        .unwrap();
        let p = s.get_peer("p").unwrap().unwrap();
        assert_eq!(
            (p.repo.as_str(), p.branch.as_str(), p.worktree_id.as_str()),
            ("weave", "feat/x", "wt-1")
        );
        let lp = &s.list_peers().unwrap()[0];
        assert_eq!(lp.repo, "weave");
        // Upsert overwrites, and a hostile newline-bearing branch is sanitized.
        s.register_peer_full(
            "p",
            "tmux",
            "%1",
            "",
            Some("/w"),
            None,
            "h",
            "weave2",
            "bad\nbranch",
            "(main)",
        )
        .unwrap();
        let p2 = s.get_peer("p").unwrap().unwrap();
        assert_eq!(p2.repo, "weave2");
        assert_eq!(
            p2.branch, "badbranch",
            "control char stripped, not rejected"
        );
        assert_eq!(p2.worktree_id, "(main)");
    }

    #[test]
    fn inbox_since_pages_forward_without_dropping_backlog() {
        let s = mem();
        let id1 = s.send("a", "b", None, "m1").unwrap();
        let id2 = s.send("a", "b", None, "m2").unwrap();
        let id3 = s.send("a", "all", None, "bcast").unwrap();

        // From 0: everything addressed to b, oldest-first, sender != b.
        let all = s.inbox_since("b", 0, 50).unwrap();
        let ids: Vec<i64> = all.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![id1, id2, id3]);

        // Strictly forward from id1: id1 excluded.
        let fwd = s.inbox_since("b", id1, 50).unwrap();
        let ids: Vec<i64> = fwd.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![id2, id3]);

        // Does not mark anything read: a real inbox still sees them unread.
        let (unread, _) = s.inbox("b", false, false, 50).unwrap();
        assert_eq!(unread.len(), 3);

        // Excludes the caller's own messages.
        assert!(s.inbox_since("a", 0, 50).unwrap().is_empty());
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

    // ---- A2 (real liveness): pid/host round-trip, migration, is_alive matrix ----

    /// `register_peer_full` round-trips the new `pid`/`host` columns through both
    /// `get_peer` and `list_peers`, and an upsert overwrites them.
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
        // list_peers carries them too.
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

    /// A DB created by a pre-A2 weave (a `peers` table WITHOUT the `pid`/`host`
    /// columns) opens and gains them in place: the migration adds them, existing
    /// rows survive, and the legacy row reads back `pid:None`, `host:""`.
    #[test]
    fn legacy_db_without_pid_host_migrates_in_place() {
        let dir =
            std::env::temp_dir().join(format!("weave-legacy-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // Build a pre-A2 peers table by hand: socket exists (pre-A2 precedent) but
        // pid/host do NOT. Insert a legacy row directly.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE peers (
                    name      TEXT PRIMARY KEY,
                    mux       TEXT NOT NULL,
                    target    TEXT NOT NULL,
                    socket    TEXT NOT NULL DEFAULT '',
                    cwd       TEXT,
                    last_seen INTEGER NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1)",
                params![now()],
            )
            .unwrap();
        }

        // Opening through SqliteStore runs migrate(): the columns are added.
        let s = SqliteStore::open(&path).unwrap();
        let p = s.get_peer("old").unwrap().unwrap();
        // Existing row survived with its original data.
        assert_eq!(p.mux, "tmux");
        assert_eq!(p.target, "%1");
        assert_eq!(p.cwd.as_deref(), Some("/legacy"));
        // New columns defaulted: pid NULL (None), host ''.
        assert_eq!(p.pid, None, "legacy row reads pid:None after migration");
        assert_eq!(p.host, "", "legacy row reads host:'' after migration");
        // Re-opening is idempotent (the guarded ALTER does not error twice).
        let s2 = SqliteStore::open(&path).unwrap();
        assert!(s2.get_peer("old").unwrap().is_some());
        // And a fresh register_peer_full now works against the upgraded table.
        s2.register_peer_full("new", "tmux", "%2", "", None, Some(7), "h", "", "", "")
            .unwrap();
        let n = s2.get_peer("new").unwrap().unwrap();
        assert_eq!(n.pid, Some(7));
        assert_eq!(n.host, "h");
    }

    /// A DB whose `peers` table predates the session-tag columns (no `repo`,
    /// `branch`, `worktree_id`) opens NON-FATALLY: `migrate()` adds the three columns
    /// in place, the legacy row survives reading back empty tags, and a peer
    /// registered with tags roundtrips them through `get_peer`/`list_peers`.
    #[test]
    fn legacy_db_without_git_tag_columns_migrates_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!(
            "weave-legacy-tags-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy-tags.db");

        // Build a peers table that has pid/host (post-A2) but NOT the three tag
        // columns, and insert a legacy row directly.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE peers (
                    name      TEXT PRIMARY KEY,
                    mux       TEXT NOT NULL,
                    target    TEXT NOT NULL,
                    socket    TEXT NOT NULL DEFAULT '',
                    cwd       TEXT,
                    last_seen INTEGER NOT NULL,
                    pid       INTEGER,
                    host      TEXT NOT NULL DEFAULT ''
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1, 42, 'h')",
                params![now()],
            )
            .unwrap();
            // Sanity: the tag columns really are absent before migration.
            assert!(!column_exists(&conn, "peers", "repo").unwrap());
            assert!(!column_exists(&conn, "peers", "branch").unwrap());
            assert!(!column_exists(&conn, "peers", "worktree_id").unwrap());
        }

        // Opening through SqliteStore runs migrate(): the 3 columns are added.
        let s = SqliteStore::open(&path).unwrap();
        let old = s.get_peer("old").unwrap().unwrap();
        // Legacy row survives; new tag columns default to "".
        assert_eq!(old.pid, Some(42));
        assert_eq!(old.host, "h");
        assert_eq!(
            (
                old.repo.as_str(),
                old.branch.as_str(),
                old.worktree_id.as_str()
            ),
            ("", "", ""),
            "legacy row reads empty tags after migration"
        );

        // A peer registered WITH tags roundtrips them at the projection positions
        // (repo/branch/worktree_id appended after host) through get_peer AND
        // list_peers.
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
            "get_peer roundtrips the migrated tag columns"
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
            "list_peers roundtrips the migrated tag columns"
        );
        // Re-opening is idempotent (the guarded ALTERs do not error twice).
        assert!(SqliteStore::open(&path)
            .unwrap()
            .get_peer("old")
            .unwrap()
            .is_some());
    }

    /// `is_alive` matrix. A fresh peer with `last_seen = now()` is recency-online;
    /// liveness then depends on pid/host:
    ///   (a) local host + dead pid + recent  => false (probe sees the gap)
    ///   (b) remote host (host != this_host) + recent => true (fail-open)
    ///   (c) NULL pid + recent => true (TTL fallback)
    ///   (d) local host + OUR OWN live pid + recent => true
    /// Plus: stale last_seen => false regardless of pid (recency guard first).
    #[test]
    fn is_alive_matrix_local_dead_remote_open_and_null_pid() {
        let base = Peer {
            name: "x".to_string(),
            mux: "tmux".to_string(),
            target: "%1".to_string(),
            socket: String::new(),
            cwd: None,
            last_seen: now(),
            pid: None,
            host: String::new(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
        };

        // (c) NULL pid + recent => true (TTL fallback, no probe).
        assert!(is_alive(&base), "null pid + recent must be alive (TTL)");

        // (b) remote host + recent => true (fail-open: cannot probe a remote PID).
        //   Use a pid that does NOT exist locally to prove the host gate (not the
        //   pid) is what keeps it alive.
        let remote = Peer {
            host: format!("{}-not-this-host", crate::config::this_host()),
            pid: Some(999_999_999),
            ..base.clone()
        };
        assert_ne!(remote.host, crate::config::this_host());
        assert!(
            is_alive(&remote),
            "remote host must fail open to alive even with an absurd pid"
        );

        // (d) local host + our OWN live pid + recent => true.
        let live_local = Peer {
            host: crate::config::this_host(),
            pid: Some(std::process::id() as i64),
            ..base.clone()
        };
        assert!(
            is_alive(&live_local),
            "local host + our own (live) pid must be alive"
        );

        // (a) local host + dead pid + recent => false (Linux probes /proc; on
        //   non-Linux pid_alive degrades to true, so only assert dead-offline where
        //   the probe is real).
        let dead_local = Peer {
            host: crate::config::this_host(),
            pid: Some(999_999_999),
            ..base.clone()
        };
        if cfg!(target_os = "linux") {
            assert!(
                !is_alive(&dead_local),
                "local host + dead pid must read offline under A2"
            );
        }

        // Recency guard wins regardless of a live pid: a stale last_seen is offline.
        let stale = Peer {
            host: crate::config::this_host(),
            pid: Some(std::process::id() as i64),
            last_seen: now() - ONLINE_TTL_SECS - 1,
            ..base.clone()
        };
        assert!(
            !is_alive(&stale),
            "stale last_seen is offline even with a live pid"
        );
    }

    /// `liveness_for` matrix with FIXED `this_host` + `now_ts` (no real
    /// hostname/clock). Covers every regime + the boundaries + the `is_alive`
    /// delegation regression-lock.
    #[test]
    fn liveness_for_matrix_fixed_host_and_now() {
        let now_ts: i64 = 1_000_000_000;
        let this = "this-host";
        let recent = now_ts; // 0s old, within the window.
        let base = Peer {
            name: "x".to_string(),
            mux: "tmux".to_string(),
            target: "%1".to_string(),
            socket: String::new(),
            cwd: None,
            last_seen: recent,
            pid: None,
            host: this.to_string(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
        };

        // same-host + live pid (our own) => AliveLocal.
        let live_local = Peer {
            pid: Some(std::process::id() as i64),
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&live_local, this, now_ts),
            Liveness::AliveLocal,
            "same-host live pid => AliveLocal"
        );

        // same-host + null pid + recent => AliveLocal (TTL fallback, no probe).
        assert_eq!(
            liveness_for(&base, this, now_ts),
            Liveness::AliveLocal,
            "same-host null pid + recent => AliveLocal"
        );

        // same-host + dead (absurd) pid + recent => Stale (pid beats recency).
        // Linux-gated: on non-Linux pid_alive degrades to true.
        let dead_local = Peer {
            pid: Some(999_999_999),
            ..base.clone()
        };
        if cfg!(target_os = "linux") {
            assert_eq!(
                liveness_for(&dead_local, this, now_ts),
                Liveness::Stale,
                "same-host dead pid + recent => Stale"
            );
        }

        // remote-host + recent + absurd pid => AliveRemote (NEVER probed).
        let remote = Peer {
            host: format!("{this}-other"),
            pid: Some(999_999_999),
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&remote, this, now_ts),
            Liveness::AliveRemote,
            "remote host + recent => AliveRemote (absurd pid NOT probed)"
        );

        // remote-host + old last_seen => Stale.
        let remote_stale = Peer {
            host: format!("{this}-other"),
            pid: Some(999_999_999),
            last_seen: now_ts - ONLINE_TTL_SECS - 1,
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&remote_stale, this, now_ts),
            Liveness::Stale,
            "remote host + old last_seen => Stale"
        );

        // empty host + recent => AliveRemote (fail-open; this_host is never empty).
        let empty_host = Peer {
            host: String::new(),
            pid: Some(999_999_999),
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&empty_host, this, now_ts),
            Liveness::AliveRemote,
            "empty host classifies as remote (fail-open)"
        );

        // this_host == peer.host boundary: exact equality flips local/remote.
        let just_remote = Peer {
            host: format!("{this}x"),
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&just_remote, this, now_ts),
            Liveness::AliveRemote,
            "host != this_host (by one char) => remote"
        );

        // TTL boundary: last_seen == now_ts - ONLINE_TTL_SECS is inclusive-alive.
        let edge_alive = Peer {
            last_seen: now_ts - ONLINE_TTL_SECS,
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&edge_alive, this, now_ts),
            Liveness::AliveLocal,
            "TTL boundary (== now - TTL) is inclusive-alive"
        );
        let edge_stale = Peer {
            last_seen: now_ts - ONLINE_TTL_SECS - 1,
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&edge_stale, this, now_ts),
            Liveness::Stale,
            "one second past the TTL boundary is Stale"
        );

        // is_alive delegation regression-lock: (liveness_for != Stale) must equal
        // the real is_alive() for the SAME peers, using the REAL this_host()/now().
        for p in [
            &base,
            &live_local,
            &dead_local,
            &remote,
            &remote_stale,
            &empty_host,
        ] {
            let bool_from_enum =
                liveness_for(p, &crate::config::this_host(), now()) != Liveness::Stale;
            assert_eq!(
                bool_from_enum,
                is_alive(p),
                "is_alive must equal (liveness_for != Stale) for {}",
                p.host
            );
        }

        // token() strings are the documented stable tokens.
        assert_eq!(Liveness::AliveLocal.token(), "alive_local");
        assert_eq!(Liveness::AliveRemote.token(), "alive_remote");
        assert_eq!(Liveness::Stale.token(), "stale");
    }

    /// `pid_alive`: our own process is alive; an absurd/unused pid (and pid<=0) is
    /// not — on Linux, where `/proc` is the real probe. On non-Linux the helper
    /// degrades to "assume alive", which is the documented contract we assert there.
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
            // Degraded contract: non-Linux assumes alive (TTL-only presence).
            assert!(pid_alive(999_999_999), "non-linux degrades to assume-alive");
        }
    }

    // ---- Tier-1 federation: read-only open is structurally write-incapable ----

    /// `open_readonly` opens an EXISTING store and can READ it, but the SQLite
    /// engine rejects any write (SQLITE_READONLY) — the structural proof of the
    /// Tier-1 read-only invariant. It also must NOT create a missing file.
    #[test]
    fn open_readonly_reads_but_cannot_write() {
        let dir = std::env::temp_dir().join(format!("weave-ro-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.db");

        // Seed a store with a peer via the normal RW open, then drop it.
        {
            let rw = SqliteStore::open(&path).unwrap();
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

        // Read-only open can list the peer.
        let ro = SqliteStore::open_readonly(&path).unwrap();
        let peers = ro.list_peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "seed");

        // But ANY write is rejected by the engine, not by convention.
        let wr =
            ro.register_peer_full("intruder", "tmux", "%2", "", None, None, "boxA", "", "", "");
        assert!(wr.is_err(), "a write through a read-only handle must error");
        let send = ro.send("a", "b", None, "x");
        assert!(
            send.is_err(),
            "a send through a read-only handle must error"
        );

        // Opening a path that does not exist read-only must NOT create it.
        let missing = dir.join("does-not-exist.db");
        assert!(SqliteStore::open_readonly(&missing).is_err());
        assert!(
            !missing.exists(),
            "read-only open must never create a missing store"
        );
    }

    /// `federated_peers` unions the local peers with a foreign read-only store,
    /// origin-tagging the foreign rows; an unreadable extra store is skipped (the
    /// local listing still returns).
    #[test]
    fn federated_peers_unions_and_isolates_failures() {
        let dir = std::env::temp_dir().join(format!("weave-fed-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let local_path = dir.join("local.db");
        let foreign_path = dir.join("foreign.db");

        let local = SqliteStore::open(&local_path).unwrap();
        local
            .register_peer_full("me", "tmux", "%1", "", None, None, "boxA", "", "", "")
            .unwrap();
        {
            let foreign = SqliteStore::open(&foreign_path).unwrap();
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
        // The foreign row is origin-tagged; the local row is Local.
        let them = views.iter().find(|v| v.peer.name == "them").unwrap();
        assert!(them.origin.is_foreign());
        let me = views.iter().find(|v| v.peer.name == "me").unwrap();
        assert_eq!(me.origin, Origin::Local);
    }

    // ---- Tier-2: outbox enqueue/list, pull cursor, and the pull driver ----

    /// `enqueue_intent` round-trips every column (incl. an empty reserved `sig`),
    /// and `list_outbox` returns only matching recipients with `id > since`, capped
    /// and oldest-first.
    #[test]
    fn enqueue_and_list_outbox_roundtrip() {
        let s = mem();
        let i1 = s
            .enqueue_intent("bob", "boxB", "alice", Some("hi"), "body1", "")
            .unwrap();
        let _i2 = s
            .enqueue_intent("carol", "", "alice", None, "for carol", "")
            .unwrap();
        let i3 = s
            .enqueue_intent("bob", "", "alice", None, "body3", "")
            .unwrap();

        // Self-inspection sees all three, oldest-first.
        let all = s.outbox_all(50).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, i1);
        assert_eq!(all[0].to, "bob");
        assert_eq!(all[0].to_host, "boxB");
        assert_eq!(all[0].from, "alice");
        assert_eq!(all[0].subject.as_deref(), Some("hi"));
        assert_eq!(all[0].body, "body1");
        assert_eq!(all[0].sig, "", "sig reserved empty in 2a");

        // list_outbox filters by recipient and id>since.
        let for_bob = s.list_outbox("bob", 0, 50).unwrap();
        let ids: Vec<i64> = for_bob.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![i1, i3], "only bob's intents, oldest-first");
        let after_first = s.list_outbox("bob", i1, 50).unwrap();
        assert_eq!(
            after_first.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![i3],
            "strictly id>since"
        );
    }

    /// `enqueue_intent` rejects oversized/invalid input (caps applied at the
    /// outbox seam, mirroring `send`).
    #[test]
    fn enqueue_intent_enforces_caps() {
        let s = mem();
        assert!(s.enqueue_intent("", "", "a", None, "x", "").is_err());
        assert!(s.enqueue_intent("b", "", "", None, "x", "").is_err());
        assert!(
            s.enqueue_intent("b", "h\nx", "a", None, "x", "").is_err(),
            "control char in to_host rejected"
        );
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s.enqueue_intent("b", "", "a", None, &big, "").is_err());
        assert!(s.enqueue_intent("b", "", "a", None, "ok", "").is_ok());
    }

    /// The per-source pull cursor defaults to 0 and round-trips through set/get.
    #[test]
    fn pull_cursor_default_and_roundtrip() {
        let s = mem();
        assert_eq!(s.pull_cursor_get("/some/src.db").unwrap(), 0);
        s.pull_cursor_set("/some/src.db", 42).unwrap();
        assert_eq!(s.pull_cursor_get("/some/src.db").unwrap(), 42);
        // Upsert overwrites.
        s.pull_cursor_set("/some/src.db", 99).unwrap();
        assert_eq!(s.pull_cursor_get("/some/src.db").unwrap(), 99);
        // Distinct sources are independent.
        assert_eq!(s.pull_cursor_get("/other.db").unwrap(), 0);
    }

    /// CRASH-WINDOW / at-least-once bound. The cursor is advanced
    /// commit-then-advance PER INTENT (not one batch transaction), so the only way
    /// to re-deliver is a crash *between* a local commit and its cursor advance.
    /// This test simulates exactly that partial-progress state — commit happened,
    /// cursor not yet advanced past it — by rewinding the cursor one intent, then
    /// re-running the pull. It asserts the re-delivery is bounded to EXACTLY the
    /// one un-acknowledged intent (at-least-once, one-intent window), NOT the whole
    /// batch, and that with the cursor correctly persisted the re-pull delivers
    /// zero (the normal path is duplicate-free).
    #[test]
    fn pull_cursor_crash_window_is_bounded_to_one_intent() {
        let dir =
            std::env::temp_dir().join(format!("weave-crash-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");

        // A enqueues THREE intents for bob (ids 1,2,3).
        {
            let a = SqliteStore::open(&a_path).unwrap();
            for n in 0..3 {
                a.enqueue_intent("bob", "", "alice", None, &format!("m{n}"), "")
                    .unwrap();
            }
        }
        let b = SqliteStore::open(&b_path).unwrap();
        let allow = vec![StoreSource::Local(a_path.clone())];
        let source = canonical_source(&a_path);

        // Normal pull commits all three; cursor now at 3.
        assert_eq!(
            pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            3
        );
        assert_eq!(b.pull_cursor_get(&source).unwrap(), 3);
        assert_eq!(b.inbox("bob", false, false, 50).unwrap().0.len(), 3);

        // Normal re-pull is duplicate-free (cursor persisted past every intent).
        assert_eq!(
            pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory())
                .unwrap()
                .committed,
            0,
            "normal re-drain must deliver nothing (no crash) — duplicate-free"
        );
        assert_eq!(b.inbox("bob", false, false, 50).unwrap().0.len(), 3);

        // Simulate a crash that committed intent #3 into the inbox but died BEFORE
        // advancing the cursor past it: rewind the cursor to 2. The next drain
        // re-reads ONLY id>2 (i.e. just #3), so the at-least-once re-delivery is
        // bounded to that single un-acknowledged intent — never the whole batch.
        b.pull_cursor_set(&source, 2).unwrap();
        let replay = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(
            replay.committed, 1,
            "a crash before the cursor advance re-delivers AT MOST the one \
             un-acknowledged intent (at-least-once, bounded), not the whole batch"
        );
        // The inbox now shows one duplicate of #3 (4 rows) — the documented, bounded
        // at-least-once cost of a real crash; the cursor is back to 3.
        assert_eq!(b.inbox("bob", false, false, 50).unwrap().0.len(), 4);
        assert_eq!(b.pull_cursor_get(&source).unwrap(), 3);
    }

    /// End-to-end pull: A enqueues an intent for B; B pulls read-only and commits
    /// it into B's own inbox; a re-pull is idempotent (no double-delivery); A is
    /// byte-unchanged across the pull (the owner-only-writes structural proof).
    #[test]
    fn pull_from_store_commits_once_and_leaves_source_unchanged() {
        let dir = std::env::temp_dir().join(format!("weave-pull-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");

        // A enqueues an intent addressed to "bob" (B's identity).
        {
            let a = SqliteStore::open(&a_path).unwrap();
            a.enqueue_intent("bob", "", "alice", Some("hi"), "hello bob", "")
                .unwrap();
        }
        // Snapshot A's bytes BEFORE B pulls.
        let before = std::fs::read(&a_path).unwrap();

        let b = SqliteStore::open(&b_path).unwrap();
        let allow = vec![StoreSource::Local(a_path.clone())];
        let pulled = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(pulled.committed, 1);

        // The message landed in B's inbox, attributed to A's `from`.
        let (rows, _) = b.inbox("bob", false, false, 50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "alice");
        assert_eq!(rows[0].body, "hello bob");

        // Re-pull is idempotent: cursor blocks the already-committed intent.
        let again = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(again.committed, 0, "re-drain must not double-deliver");
        let (rows2, _) = b.inbox("bob", false, false, 50).unwrap();
        assert_eq!(rows2.len(), 1, "still exactly one inbox row");

        // OWNER-ONLY-WRITES: A's file is byte-identical after the pulls.
        let after = std::fs::read(&a_path).unwrap();
        assert_eq!(
            before, after,
            "pulling must leave the source store byte-unchanged"
        );
    }

    /// An unreadable/missing source is skipped (best-effort), and an intent
    /// addressed to someone else is not committed.
    #[test]
    fn pull_skips_bad_source_and_misaddressed_intents() {
        let dir =
            std::env::temp_dir().join(format!("weave-pull2-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");
        {
            let a = SqliteStore::open(&a_path).unwrap();
            // Addressed to carol, NOT to bob — must never reach bob's inbox.
            a.enqueue_intent("carol", "", "alice", None, "not for bob", "")
                .unwrap();
        }
        let b = SqliteStore::open(&b_path).unwrap();
        let allow = vec![
            StoreSource::Local(dir.join("missing.db")),
            StoreSource::Local(a_path.clone()),
        ];
        let pulled = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(pulled.committed, 0);
        assert_eq!(pulled.sources_skipped, 1, "missing source skipped");
        let (rows, _) = b.inbox("bob", false, false, 50).unwrap();
        assert!(rows.is_empty(), "a misaddressed intent is never committed");
    }

    /// A legacy DB created before Tier-2 (no `outbox`/`pull_cursor`) gains both
    /// tables on open and is fully usable.
    #[test]
    fn legacy_db_gains_tier2_tables() {
        let dir =
            std::env::temp_dir().join(format!("weave-t2-legacy-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-Tier-2 store: only messages exists.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT, body TEXT NOT NULL
                 );",
            )
            .unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        // The new tables exist and work.
        let id = s.enqueue_intent("bob", "", "alice", None, "x", "").unwrap();
        assert!(id > 0);
        assert_eq!(s.pull_cursor_get("src").unwrap(), 0);
        s.pull_cursor_set("src", 7).unwrap();
        assert_eq!(s.pull_cursor_get("src").unwrap(), 7);
        // The 2d `keys` table is also present on the upgraded legacy store.
        assert!(s.get_key("alice").unwrap().is_none());
        s.register_key("alice", "deadbeef").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("deadbeef"));
    }

    /// The `keys` table round-trips a registered pubkey through get/list, upserts on
    /// conflict, and rejects an invalid identity. The table is plain data — present
    /// in every build regardless of the `sign` feature.
    #[test]
    fn keys_register_get_list_roundtrip() {
        let s = mem();
        assert!(s.get_key("alice").unwrap().is_none(), "unknown key ⇒ None");
        s.register_key("alice", "aa11").unwrap();
        s.register_key("bob", "bb22").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));

        // Upsert overwrites the pubkey for an existing identity.
        s.register_key("alice", "cc33").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("cc33"));

        // list_keys returns all pairs ordered by identity.
        let keys = s.list_keys().unwrap();
        assert_eq!(
            keys,
            vec![
                ("alice".to_string(), "cc33".to_string()),
                ("bob".to_string(), "bb22".to_string()),
            ]
        );

        // An invalid identity is rejected at the seam.
        assert!(s.register_key("", "00").is_err());
        assert!(s.register_key("a\nb", "00").is_err());
    }

    /// Signed-identity commit gate (2d, `sign` feature) end-to-end through
    /// `pull_from_store`:
    ///   - a VALID signature ⇒ committed;
    ///   - a FORGED/tampered signature ⇒ ALWAYS rejected (strict or not);
    ///   - an UNSIGNED intent ⇒ committed under advisory (default), DROPPED under
    ///     strict_verify.
    #[cfg(feature = "sign")]
    #[test]
    fn signed_pull_verifies_commits_and_rejects_forgery() {
        use crate::sign::{sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let signer = SigningKey::from_bytes(&[42u8; 32]);
        let pubkey = to_hex(signer.verifying_key().as_bytes());

        // Helper: enqueue an intent into a fresh source store, return it as a
        // Local StoreSource so it can be passed straight to `pull_from_store`.
        fn src_with(
            dir: &std::path::Path,
            tag: &str,
            from: &str,
            body: &str,
            sig: &str,
        ) -> StoreSource {
            let p = dir.join(format!("{tag}.db"));
            let a = SqliteStore::open(&p).unwrap();
            a.enqueue_intent("bob", "", from, None, body, sig).unwrap();
            StoreSource::Local(p)
        }

        let dir = std::env::temp_dir().join(format!("weave-sign-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();

        // (1) Valid signature from "alice" — B has alice's key registered.
        let good_sig = sign_intent(&signer, "alice", "bob", "hi");
        let good = src_with(&dir, "good", "alice", "hi", &good_sig);

        // (2) Forged: a signature that does NOT match the (from,to,body).
        let forged = src_with(&dir, "forged", "alice", "tampered", &good_sig);

        // (3) Unsigned intent.
        let unsigned = src_with(&dir, "unsigned", "carol", "plain", "");

        // --- non-strict receiver ---
        {
            let b = SqliteStore::open(&dir.join("b1.db")).unwrap();
            b.register_key("alice", &pubkey).unwrap();
            // Valid sig commits.
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&good),
                    &VerifyPolicy::strict(false)
                )
                .unwrap()
                .committed,
                1,
                "a valid signature commits"
            );
            // Forged sig is rejected even in non-strict mode.
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&forged),
                    &VerifyPolicy::strict(false)
                )
                .unwrap()
                .committed,
                0,
                "a forged signature is ALWAYS rejected"
            );
            // Unsigned commits under advisory fallback.
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&unsigned),
                    &VerifyPolicy::strict(false)
                )
                .unwrap()
                .committed,
                1,
                "unsigned commits under advisory (non-strict)"
            );
        }

        // --- strict receiver ---
        {
            let b = SqliteStore::open(&dir.join("b2.db")).unwrap();
            b.register_key("alice", &pubkey).unwrap();
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&good),
                    &VerifyPolicy::strict(true)
                )
                .unwrap()
                .committed,
                1,
                "a valid signature commits even under strict"
            );
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&forged),
                    &VerifyPolicy::strict(true)
                )
                .unwrap()
                .committed,
                0,
                "a forged signature is rejected under strict too"
            );
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&unsigned),
                    &VerifyPolicy::strict(true)
                )
                .unwrap()
                .committed,
                0,
                "an unsigned intent is DROPPED under strict_verify"
            );
        }
    }

    /// The NEW trust-set-aware decision table (2d), every cell, with FIXED-seed keys
    /// (never OsRng) for determinism. Covers: trusted+good/bad/unsigned,
    /// untrusted+good/bad/unsigned, no-trust-set, global forced/disabled, and the
    /// R1 absolute-revocation cases (revoked-signed rejected EVEN with Some(false)).
    #[cfg(feature = "sign")]
    #[test]
    fn verify_decision_table_every_cell() {
        use crate::sign::{fingerprint, sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let alice = SigningKey::from_bytes(&[10u8; 32]);
        let alice_pk = to_hex(alice.verifying_key().as_bytes());
        let alice_fp = fingerprint(&alice_pk).unwrap(); // display only; trust uses full
        let alice_full = format!(
            "SHA256:{}",
            crate::sign::fingerprint_full(&alice_pk).unwrap()
        );
        let _ = alice_fp;

        let dir =
            std::env::temp_dir().join(format!("weave-dtable-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tag = 0u64;
        // Build a fresh source store containing one intent and return it.
        let mut src = |from: &str, body: &str, sig: &str| -> StoreSource {
            tag += 1;
            let p = dir.join(format!("s{tag}.db"));
            let a = SqliteStore::open(&p).unwrap();
            a.enqueue_intent("bob", "", from, None, body, sig).unwrap();
            StoreSource::Local(p)
        };
        // Fresh receiver B with alice's key registered.
        let rcv = |n: u32| -> SqliteStore {
            let b = SqliteStore::open(&dir.join(format!("b{n}.db"))).unwrap();
            b.register_key("alice", &alice_pk).unwrap();
            b
        };
        let committed = |b: &SqliteStore, s: &StoreSource, policy: &VerifyPolicy| -> usize {
            pull_from_store(b, "bob", std::slice::from_ref(s), policy)
                .unwrap()
                .committed
        };

        let good = sign_intent(&alice, "alice", "bob", "hi");
        let trust_alice = VerifyPolicy {
            strict_override: None,
            trust: vec![alice_full.clone()],
            revoked: vec![],
        };

        // --- trust set configured, alice TRUSTED, no override ---
        // trusted + good sig ⇒ COMMIT
        assert_eq!(
            committed(&rcv(1), &src("alice", "hi", &good), &trust_alice),
            1,
            "trusted+good commits"
        );
        // trusted + bad sig ⇒ REJECT (present-but-invalid always)
        assert_eq!(
            committed(&rcv(2), &src("alice", "TAMPER", &good), &trust_alice),
            0,
            "trusted+bad rejected"
        );
        // trusted + unsigned ⇒ REJECT (NEW: trusted ⇒ strict)
        assert_eq!(
            committed(&rcv(3), &src("alice", "hi", ""), &trust_alice),
            0,
            "trusted+unsigned rejected"
        );

        // --- trust set configured, sender UNTRUSTED (carol not in trust) ---
        let carol = SigningKey::from_bytes(&[20u8; 32]);
        let carol_pk = to_hex(carol.verifying_key().as_bytes());
        let carol_good = sign_intent(&carol, "carol", "bob", "yo");
        let rcv_carol = |n: u32| -> SqliteStore {
            let b = SqliteStore::open(&dir.join(format!("bc{n}.db"))).unwrap();
            b.register_key("alice", &alice_pk).unwrap();
            b.register_key("carol", &carol_pk).unwrap();
            b
        };
        // untrusted + good sig ⇒ COMMIT (advisory: verified, just not in trust set)
        assert_eq!(
            committed(
                &rcv_carol(1),
                &src("carol", "yo", &carol_good),
                &trust_alice
            ),
            1,
            "untrusted+good commits"
        );
        // untrusted + bad sig ⇒ REJECT (present-but-invalid always)
        assert_eq!(
            committed(
                &rcv_carol(2),
                &src("carol", "BAD", &carol_good),
                &trust_alice
            ),
            0,
            "untrusted+bad rejected"
        );
        // untrusted + unsigned ⇒ COMMIT (advisory; unsigned operation preserved)
        assert_eq!(
            committed(&rcv_carol(3), &src("carol", "yo", ""), &trust_alice),
            1,
            "untrusted+unsigned commits"
        );

        // --- NO trust set (default) ---
        let no_trust = VerifyPolicy::default();
        assert_eq!(
            committed(&rcv(4), &src("alice", "hi", ""), &no_trust),
            1,
            "no-trust-set unsigned commits (unchanged)"
        );
        assert_eq!(
            committed(&rcv(5), &src("alice", "BAD", &good), &no_trust),
            0,
            "no-trust-set bad sig rejected"
        );

        // --- global override forced/disabled ---
        let forced = VerifyPolicy::strict(true);
        assert_eq!(
            committed(&rcv(6), &src("alice", "hi", ""), &forced),
            0,
            "global forced ⇒ unsigned rejected"
        );
        let disabled = VerifyPolicy::strict(false);
        assert_eq!(
            committed(&rcv(7), &src("alice", "hi", ""), &disabled),
            1,
            "global disabled ⇒ unsigned commits"
        );
        // Even with a trust set, a forced override applies to everyone.
        let forced_trust = VerifyPolicy {
            strict_override: Some(true),
            trust: vec![alice_full.clone()],
            revoked: vec![],
        };
        assert_eq!(
            committed(&rcv(8), &src("carol", "yo", ""), &forced_trust),
            0,
            "forced ⇒ even untrusted unsigned rejected"
        );

        // --- R1 absolute revocation: a VALID signature against a REVOKED key ---
        let revoke_alice = VerifyPolicy {
            strict_override: None,
            trust: vec![],
            revoked: vec![alice_full.clone()],
        };
        assert_eq!(
            committed(&rcv(9), &src("alice", "hi", &good), &revoke_alice),
            0,
            "revoked + good sig REJECTED"
        );
        // R1 hard case: revoked + good sig is STILL rejected even with Some(false).
        let revoke_disabled = VerifyPolicy {
            strict_override: Some(false),
            trust: vec![],
            revoked: vec![alice_full.clone()],
        };
        assert_eq!(
            committed(&rcv(10), &src("alice", "hi", &good), &revoke_disabled),
            0,
            "R1: revoked key's SIGNED message rejected even when strict disabled"
        );
        // But an UNSIGNED message merely claiming a revoked sender may relax to
        // advisory under Some(false) (the toggle governs the unsigned path).
        assert_eq!(
            committed(&rcv(11), &src("alice", "hi", ""), &revoke_disabled),
            1,
            "R1: unsigned claim under Some(false) relaxes to advisory"
        );

        // --- SIGNED but NO REGISTERED KEY (plan cell "Trusted + signed, no key"):
        // a present signature we cannot check (sender has no registered pubkey) is
        // "present but unverifiable" — it CANNOT be trusted (no fp to match), so it
        // follows the advisory path: COMMIT under advisory, DROP under forced-strict.
        let rcv_nokey = |n: u32| -> SqliteStore {
            // A receiver that has NO key for "alice" (do not register one).
            SqliteStore::open(&dir.join(format!("bn{n}.db"))).unwrap()
        };
        // advisory (no trust set): a signed-but-uncheckable intent commits.
        assert_eq!(
            committed(&rcv_nokey(1), &src("alice", "hi", &good), &no_trust),
            1,
            "signed + no registered key ⇒ advisory commit (cannot be trusted, sig ignored)"
        );
        // forced strict: a signed-but-uncheckable intent is dropped.
        assert_eq!(
            committed(&rcv_nokey(2), &src("alice", "hi", &good), &forced),
            0,
            "signed + no registered key ⇒ dropped under forced strict"
        );
        // trust set configured but this sender has no key ⇒ cannot match trust ⇒
        // advisory commit (a trust entry without a registered key never grants trust).
        assert_eq!(
            committed(&rcv_nokey(3), &src("alice", "hi", &good), &trust_alice),
            1,
            "signed + no key under a trust set ⇒ untrusted ⇒ advisory commit"
        );
    }

    /// Rotation overlap (R6, config-based): trusting BOTH old and new fingerprints
    /// lets messages signed by EITHER key verify during the window; revoking the OLD
    /// fingerprint then makes the old key's SIGNED messages fail while the new key's
    /// still commit. Fixed-seed keys for determinism.
    #[cfg(feature = "sign")]
    #[test]
    fn rotation_overlap_then_revoke_old() {
        use crate::sign::{fingerprint_full, sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let old = SigningKey::from_bytes(&[30u8; 32]);
        let new = SigningKey::from_bytes(&[31u8; 32]);
        let old_pk = to_hex(old.verifying_key().as_bytes());
        let new_pk = to_hex(new.verifying_key().as_bytes());
        let old_fp = format!("SHA256:{}", fingerprint_full(&old_pk).unwrap());
        let new_fp = format!("SHA256:{}", fingerprint_full(&new_pk).unwrap());

        let dir =
            std::env::temp_dir().join(format!("weave-rotate-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tag = 0u64;
        let mut src = |pk_signer: &SigningKey, body: &str| -> StoreSource {
            tag += 1;
            let p = dir.join(format!("r{tag}.db"));
            let a = SqliteStore::open(&p).unwrap();
            let sig = sign_intent(pk_signer, "alice", "bob", body);
            a.enqueue_intent("bob", "", "alice", None, body, &sig)
                .unwrap();
            StoreSource::Local(p)
        };

        // Overlap: B trusts BOTH fps, but "alice" can only register ONE pubkey at a
        // time (PRIMARY KEY upsert). During overlap the receiver keeps the OLD pubkey
        // registered (per the rotate guidance) so old-key messages still verify.
        let overlap = VerifyPolicy {
            strict_override: None,
            trust: vec![old_fp.clone(), new_fp.clone()],
            revoked: vec![],
        };

        // While "alice" is registered to the OLD key, an old-key signature commits.
        {
            let b = SqliteStore::open(&dir.join("b_old.db")).unwrap();
            b.register_key("alice", &old_pk).unwrap();
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&src(&old, "during-overlap")),
                    &overlap
                )
                .unwrap()
                .committed,
                1,
                "old key still verifies during overlap (old pubkey registered + trusted)"
            );
        }
        // After re-registering "alice" to the NEW key, a new-key signature commits.
        {
            let b = SqliteStore::open(&dir.join("b_new.db")).unwrap();
            b.register_key("alice", &new_pk).unwrap();
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&src(&new, "post-rotate")),
                    &overlap
                )
                .unwrap()
                .committed,
                1,
                "new key verifies once registered + trusted"
            );
        }
        // Revoke the OLD fp: while "alice" is still on the OLD key, its SIGNED message
        // is rejected (revocation wins), but the NEW key (registered) still commits.
        let revoke_old = VerifyPolicy {
            strict_override: None,
            trust: vec![new_fp.clone()],
            revoked: vec![old_fp.clone()],
        };
        {
            let b = SqliteStore::open(&dir.join("b_revold.db")).unwrap();
            b.register_key("alice", &old_pk).unwrap();
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&src(&old, "old-after-revoke")),
                    &revoke_old
                )
                .unwrap()
                .committed,
                0,
                "old key's signed message rejected after its fp is revoked"
            );
        }
        {
            let b = SqliteStore::open(&dir.join("b_revnew.db")).unwrap();
            b.register_key("alice", &new_pk).unwrap();
            assert_eq!(
                pull_from_store(
                    &b,
                    "bob",
                    std::slice::from_ref(&src(&new, "new-after-revoke")),
                    &revoke_old
                )
                .unwrap()
                .committed,
                1,
                "new key still commits after the old fp is revoked"
            );
        }
    }
}
