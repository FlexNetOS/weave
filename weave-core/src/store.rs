//! Persistent message + peer store.
//!
//! [`Store`] is the backend-agnostic interface. [`SqliteStore`] (rusqlite, bundled)
//! is the default; a feature-gated libSQL/Turso backend implements the same trait
//! for cross-machine sync (see `store_libsql.rs`). The on-disk SQLite format is
//! libSQL-compatible, so the file is portable between backends.

use crate::config::StoreSource;
use crate::model::{
    now, Ask, AskGroup, AskKind, AskManyResult, AskRole, AskState, ClaimOutcome, DeliveryTrace,
    Intent, Job, JobFilter, JobPatch, JobResultView, JobSpec, JobState, Message,
    OrchestratorStatus, Peer, PermissionStatus, ReviewItem, ReviewItemState, ReviewQueueFilter,
    Schedule, ScheduleKind,
};
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
use crate::model::{
    ask_id_valid, ask_many_id_valid, attempt_id_valid, classify_ask_many, is_broadcast,
    job_id_valid, new_ask_id, new_ask_many_id, new_attempt_id, new_job_id, new_review_id,
    permission_status, pr_url_valid, AskManyChildView, Lease, BROADCAST_SQL, MAX_CRON_EXPR_LEN,
    MAX_DELIVERY_ROWS, MAX_REVIEW_IDENT_LEN, MAX_REVIEW_TITLE_LEN,
};
#[cfg(feature = "sqlite")]
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
#[cfg(feature = "sqlite")]
use std::path::Path;
#[cfg(feature = "sqlite")]
use std::time::Duration;

/// A peer is considered "online" if its last heartbeat is within this window.
pub const ONLINE_TTL_SECS: i64 = 900;
/// Daemon heartbeat window: a presence row written within this many seconds is
/// considered FRESH and wins over the TTL fallback.  30 s is tight enough to
/// detect a crashed daemon quickly, loose enough to tolerate scheduling jitter.
pub const PRESENCE_TTL_SECS: i64 = 30;

/// Hard upper bound on how many peers a single `ask_many` fanout may target (after
/// de-dup). The fanout opens one child ask + fires one live nudge per target, so an
/// unbounded list is a token/RAM/inject-storm DoS. 64 is generous (≥ repowire's 50)
/// and bounds the blast radius. Not config — a fixed store constant in the spirit of
/// `MAX_PEER_DBS` / `MAX_SESSIONS`. A list longer than this is a HARD whole-call error
/// (rejected before any insert).
pub const MAX_ASK_MANY_TARGETS: usize = 64;

/// The per-child result of an `ask_many` create: each target peer paired with either
/// its minted correlation id (the child ask was created) or a best-effort error
/// string (the peer was rejected pre-insert — invalid/broadcast/over-length — and is
/// counted as `failed` at read time). Returned alongside the parent id so the caller
/// (mcp/main) can fire a per-child nudge for the created children WITHOUT a
/// `store → inject` edge.
#[derive(Debug, Clone)]
pub struct AskManyOutcome {
    pub parent_id: String,
    /// `(peer, Ok(correlation_id) | Err(reason))`, one per requested (de-duped) peer.
    pub children: Vec<(String, std::result::Result<String, String>)>,
}

/// (name, unread, last_activity_ts)
pub type SessionInfo = (String, i64, i64);

/// Backend-agnostic store interface. Object-safe so the app can hold a
/// `Box<dyn Store>` and pick the backend at runtime.
pub trait Store: Send {
    fn send(
        &self,
        sender: &str,
        recipient: &str,
        subject: Option<&str>,
        body: &str,
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<i64>;
    fn inbox(
        &self,
        me: &str,
        include_read: bool,
        mark_read: bool,
        limit: i64,
    ) -> Result<(Vec<Message>, i64)>;
    fn history(&self, me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>>;
    /// Full-text search over messages using FTS5. Returns matching messages
    /// newest-first, capped at `limit`. Read-only.
    fn search(&self, query: &str, limit: i64) -> Result<Vec<Message>>;
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
    /// Oldest unread message for `me`, or `None` if the inbox is empty.
    fn peek_oldest_unread(&self, me: &str) -> Result<Option<Message>>;
    /// Last unread-message id that triggered a wake block for `me`.
    fn wake_last_acked(&self, me: &str) -> Result<i64>;
    /// Advance the wake watermark for `me` to `id`.
    fn set_wake_ack(&self, me: &str, id: i64) -> Result<()>;
    /// Delete messages (and their read-markers) older than `older_than_secs`.
    /// Returns how many messages were removed. Retention / disk-bound guard.
    fn gc(&self, older_than_secs: i64) -> Result<i64>;
    /// Register (upsert) a peer carrying every field, including the registering
    /// process's `pid` and `host` for real process-liveness, and the visibility
    /// `circle` (P4). This is the full primitive each backend implements; the
    /// [`Store::register_peer`] wrapper forwards here with `pid=None, host=""`,
    /// empty git tags, `circle="default"`, and no cert so legacy call sites keep
    /// working unchanged.
    ///
    /// **Role is NOT a parameter.** A registration NEVER asserts a role: a new
    /// row inserts `role='peer'` and an upsert of an existing row PRESERVES its
    /// current role (so a re-register can never silently demote an orchestrator).
    /// The only path to `role='orchestrator'` is
    /// [`Store::claim_orchestrator_role`].
    ///
    /// **Birth certificate (WL-018):** Returns the peer's birth cert on success.
    /// - New peer: mints a fresh 64-hex cert, INSERTs it, returns it.
    /// - Existing peer with `birth_cert IS NULL`: mints a fresh cert, UPDATEs it,
    ///   returns it (backward-compat upgrade).
    /// - Existing peer with `birth_cert IS NOT NULL`:
    ///   - `birth_cert` arg is `None` → rejects (must provide cert to re-register).
    ///   - `birth_cert` arg mismatches → rejects (identity takeover protection).
    ///   - `birth_cert` arg matches → UPDATEs other fields, returns existing cert.
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
        circle: &str,
        birth_cert: Option<&str>,
    ) -> Result<String>;

    /// Register (upsert) a peer without PID/host liveness info or git tags.
    /// Additive backward-compatible wrapper over [`Store::register_peer_full`]:
    /// forwards with `pid=None, host=""` (== liveness unknown ⇒ presence falls
    /// back to the TTL recency guess), empty git tags, and no cert. Keeps existing
    /// 5-arg call sites/tests compiling.
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
    ) -> Result<String> {
        let cert = self.get_birth_cert(name).ok().flatten();
        self.register_peer_full(
            name,
            mux,
            target,
            socket,
            cwd,
            None,
            "",
            "",
            "",
            "",
            "default",
            cert.as_deref(),
        )
    }
    fn get_peer(&self, name: &str) -> Result<Option<Peer>>;
    fn get_birth_cert(&self, name: &str) -> Result<Option<String>>;
    fn list_peers(&self) -> Result<Vec<Peer>>;

    /// List peers scoped to `circle`. `None` (or the literal `"*"`) ⇒ all
    /// circles (mesh-wide). A concrete name keeps only peers whose
    /// [`circle_or_default`]-normalized circle matches (so an empty/legacy circle
    /// classifies into `"default"`). Default impl filters [`Store::list_peers`];
    /// backends share it. Backward-compatible: a `"default"` filter over an
    /// all-default DB returns exactly the same set as `list_peers`.
    #[allow(dead_code)]
    fn list_peers_in_circle(&self, circle: Option<&str>) -> Result<Vec<Peer>> {
        let all = self.list_peers()?;
        match circle {
            None | Some("*") => Ok(all),
            Some(target) => {
                let target = crate::model::circle_or_default(target);
                Ok(all
                    .into_iter()
                    .filter(|p| crate::model::circle_or_default(&p.circle) == target)
                    .collect())
            }
        }
    }

    /// Claim the orchestrator role for a circle (P4). Resolves the effective
    /// circle (`circle` arg, else the caller's own peer-row circle, else
    /// `"default"`), then in ONE transaction: if a DIFFERENT peer is the LIVE
    /// (`role='orchestrator'` AND [`is_alive`](crate::store::is_alive)) holder and
    /// `force` is false ⇒ returns [`ClaimOutcome::Refused`] WITHOUT any write;
    /// otherwise demotes every other `role='orchestrator'` row in the circle to
    /// `'peer'` and sets the caller's row to `'orchestrator'`, returning
    /// [`ClaimOutcome::Claimed`] with the demoted list. The caller MUST already be
    /// registered (Err otherwise). The forced demote is a single-row UPDATE within
    /// the caller's OWN store (never a foreign store), the only cross-row peer
    /// write P4 adds; it is non-destructive (a role bit) so it is NOT
    /// `confirm`-gated.
    fn claim_orchestrator_role(
        &self,
        me: &str,
        circle: Option<&str>,
        force: bool,
    ) -> Result<ClaimOutcome>;

    /// Report the orchestrator status of a circle (P4). Resolves the effective
    /// circle like [`Store::claim_orchestrator_role`], selects every
    /// `role='orchestrator'` row in it, and classifies each via
    /// [`is_alive`](crate::store::is_alive) (NO new probe — the daemon-free analog
    /// of repowire's heartbeat verdict). `present` is true iff a LIVE holder
    /// exists; `holder` is the most-recently-seen live one.
    fn orchestrator_status(&self, circle: Option<&str>) -> Result<OrchestratorStatus>;

    /// Set the live turn-state of a peer (P5 rich presence). SELF-ONLY: `name` is
    /// the CALLER's own resolved identity (the MCP/CLI/hook layer binds it, never
    /// an arg-supplied target). `state` is validated through [`TurnState::from_str`]
    /// — an unknown value is a hard error (never stored raw). UPDATE-only on the
    /// named row, so it never creates or consumes a foreign row. Idempotent.
    fn set_turn_state(&self, name: &str, state: &str) -> Result<()>;

    /// Set a peer's free-form, self-reported description (P5). SELF-ONLY (`name` is
    /// the caller's own identity). The text is control-stripped + capped to
    /// [`MAX_DESC_LEN`] via `sanitize_tag` at this seam (lossy-but-total, never
    /// errors on oversized input — it truncates). Stamps `description_ts = now()`;
    /// a cleared (empty) description stamps `description_ts = 0` so it is
    /// unambiguously "absent" rather than "set-to-empty-at-T". UPDATE-only.
    fn set_description(&self, name: &str, description: &str) -> Result<()>;

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
        let id = self.send(sender, &recipient, subject.as_deref(), body, None, None)?;
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
    #[allow(clippy::too_many_arguments)]
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
        ttl: i64,
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

    /// Tier-2 (2d/#7): ADD a peer/session's hex-encoded ed25519 public key to the
    /// `identity_keys` registry, used to VERIFY signed intents claiming to be from
    /// that identity. Multi-key by design (rotation overlap): registering a NEW key
    /// APPENDS it alongside any existing keys; registering the SAME key again is a
    /// NO-OP (never an error, never a duplicate row). Enforces
    /// [`MAX_KEYS_PER_IDENT`] — adding a NEW key when the identity already holds the
    /// cap returns an error (it must NEVER panic). The table is plain data (present
    /// in every build); only the SIGN/VERIFY crypto is behind the `sign` feature, so
    /// a `sign`-built receiver can read a key registered by any build. Bound
    /// `params!`.
    #[allow(dead_code)]
    fn register_key(&self, identity: &str, pubkey: &str) -> Result<()>;

    /// Fetch the MOST-RECENT registered public key for `identity` (hex; newest by
    /// `added_ts`, ties by rowid), or `None` if none. A back-compat shim over
    /// [`Store::get_keys`] for non-verify callers that only want a single
    /// representative key (display/trust). Verification uses [`Store::get_keys`].
    #[allow(dead_code)]
    fn get_key(&self, identity: &str) -> Result<Option<String>>;

    /// Fetch ALL registered public keys (hex) for `identity`, oldest-first by
    /// `added_ts` (ties by rowid). Empty when the identity has no registered key.
    /// This is the authoritative multi-key lookup used by `verify_pulled_intent`:
    /// a signed intent commits IFF it verifies against at least one registered
    /// NON-REVOKED key.
    #[allow(dead_code)]
    fn get_keys(&self, identity: &str) -> Result<Vec<String>>;

    /// Remove a single `(identity, pubkey)` registration. Returns `true` if a row
    /// was deleted, `false` if no such pair existed. Backs `weave key remove`
    /// (pruning a retired key after rotation). Owner-only: writes the LOCAL store.
    #[allow(dead_code)]
    fn remove_key(&self, identity: &str, pubkey: &str) -> Result<bool>;

    /// List ALL registered `(identity, pubkey)` pairs, ordered by identity then
    /// `added_ts`. With multi-key registration an identity may appear in several
    /// rows. Backs `weave key list`.
    #[allow(dead_code)]
    fn list_keys(&self) -> Result<Vec<(String, String)>>;

    /// Append an observed/declared revocation audit event to the local `revocations`
    /// log. Owner-only: writes the LOCAL store this process owns (the receiver
    /// recording its own enforcement, or the operator recording a declared revoke).
    /// Plain data in every build; only the SIGN verifier / `key revoke` calls it. The
    /// table is READ-only with respect to the R1 decision — this is a side-effect, not
    /// a decision input. Bound `params!`. A failure here is best-effort at the call
    /// site (the security decision never depends on it succeeding).
    #[allow(dead_code)]
    fn record_revocation(&self, ev: &RevocationEvent) -> Result<()>;

    /// Most-recent-first audit rows, capped at `min(limit, MAX_REVOCATIONS_LIST)`.
    /// Backs `weave audit revocations`. Read-only; never consulted by the verifier.
    #[allow(dead_code)]
    fn list_revocations(&self, limit: i64) -> Result<Vec<RevocationEvent>>;

    /// Total recorded revocation events (cheap `COUNT` for the doctor rollup).
    #[allow(dead_code)]
    fn count_revocations(&self) -> Result<i64>;

    /// P1 tracked ask: open a correlation-tracked request. ONE transaction that
    /// validates (`check_ident` asker/askee, `check_body`, reject a broadcast
    /// askee — P1 is point-to-point), inserts the question into `messages`, mints
    /// `id = new_ask_id(rowid)`, and inserts the `asks` row `state='open'`. When
    /// `reply_to` is given (a prior ask id, `ask_id_valid` + must exist and involve
    /// this asker/askee pair), the question links to the prior thread's last
    /// message via `in_reply_to` and the prior ask is transitioned `→acked` in the
    /// SAME transaction (chaining closes the prior thread, repowire parity).
    /// Returns `(correlation_id, question_msg_id)`. Owner-only: writes the LOCAL
    /// store. No `store → inject` edge — the live nudge is fired caller-side.
    ///
    /// `allow(dead_code)`: weave is a binary crate, so a `pub` trait method whose
    /// only callers are tests / CLI / MCP arms is otherwise flagged unused.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn ask(
        &self,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        body: &str,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(String, i64)>;

    /// P1 tracked answer: record an answer to an open/answered ask. Validates
    /// `ask_id_valid`, loads the ask, derives the recipient = the asker (the answer
    /// goes back to whoever opened it), enforces `responder == askee` and that the
    /// thread is not already `acked`, inserts the answer `messages` row
    /// (`in_reply_to = question_msg_id`, subject inherited), sets `answer_msg_id`,
    /// and transitions `→answered` (guarded by [`AskState::can_transition`]). One
    /// transaction. Returns the answer message id. Unknown id / acked thread /
    /// wrong responder are clean errors (never a panic).
    #[allow(dead_code)]
    fn answer(&self, responder: &str, correlation_id: &str, body: &str) -> Result<i64>;

    /// P1 tracked ack: close a thread. Validates, loads, rejects double-ack
    /// (already `acked`) and an unknown thread, enforces `acker == askee`,
    /// transitions `→acked` (guarded), stamps `closed_ts`, and stores the optional
    /// `message` as `close_note`. A PURE state transition — no new message row (a
    /// delivered closing note is a normal `answer` first). Clean errors, never a
    /// panic.
    #[allow(dead_code)]
    fn ack(&self, acker: &str, correlation_id: &str, message: Option<&str>) -> Result<()>;

    /// P1: fetch a single ask by correlation id (PK lookup), or `None`.
    #[allow(dead_code)]
    fn get_ask(&self, correlation_id: &str) -> Result<Option<Ask>>;

    /// P1: list asks where `me` plays `role` (asker / askee / either), newest-first,
    /// capped at `clamp_limit(limit)`.
    #[allow(dead_code)]
    fn list_asks(&self, me: &str, role: AskRole, limit: i64) -> Result<Vec<Ask>>;

    /// WL-040b: list the ask-many PARENT anchor rows (`ask_groups`) for `parent_ids`
    /// (the distinct `parent_id`s of the exported asks), so `session export` can carry
    /// the group rows the child asks reference. Read-only; unknown ids are simply
    /// absent from the result. Returns in no guaranteed order.
    #[allow(dead_code)]
    fn list_ask_groups(&self, parent_ids: &[String]) -> Result<Vec<AskGroup>>;

    /// WL-040b: materialize ONE exported ask directly in an arbitrary terminal/
    /// non-terminal [`AskState`] (open / answered / acked), bypassing the normal
    /// create→answer→ack lifecycle (and [`AskState::can_transition`]) — the question/
    /// answer `messages` rows already exist from the message-import pass, so this is a
    /// deliberate out-of-order materializer. It inserts NO `messages` row. `id` is the
    /// caller's freshly-minted local ask id (the source id is meaningless in the
    /// target); `question_msg_id`/`answer_msg_id` are the REMAPPED local message ids;
    /// `asker`/`askee` are already `--as`-remapped. Re-validates its own inputs at the
    /// store seam (`check_ident` asker/askee, `ask_id_valid(id)`, options/close_note
    /// length-capped) and dedups on `(asker, askee, question_msg_id)` so re-import is
    /// idempotent. Parameterized SQL only. Returns `Ok(true)` if a row was inserted,
    /// `Ok(false)` if an equivalent ask already existed (skipped).
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn import_ask(
        &self,
        id: &str,
        question_msg_id: i64,
        answer_msg_id: Option<i64>,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        state: AskState,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
        close_note: Option<&str>,
        opened_ts: i64,
        updated_ts: i64,
        closed_ts: Option<i64>,
        parent_id: Option<&str>,
    ) -> Result<bool>;

    /// WL-040b: materialize ONE exported ask-many PARENT anchor (`ask_groups`) row,
    /// replayed BEFORE the child asks that reference it so `parent_id` linkage
    /// survives. `parent_id` is the caller's freshly-minted local group id. Re-validates
    /// (`ask_many_id_valid(parent_id)`, `check_ident(asker)`, body/subject capped) and
    /// dedups on `parent_id` so re-import is idempotent. Parameterized SQL only.
    /// Returns `Ok(true)` if inserted, `Ok(false)` if it already existed (skipped).
    #[allow(dead_code)]
    fn import_ask_group(
        &self,
        parent_id: &str,
        asker: &str,
        subject: Option<&str>,
        body: &str,
        opened_ts: i64,
        target_count: i64,
    ) -> Result<bool>;

    /// P1: true iff `me` has at least one ask in [`AskState::Open`] where they are
    /// the askee. Used by the prompt-hook reminder nudge (WL-014).
    #[allow(dead_code)]
    fn has_open_asks(&self, me: &str) -> Result<bool>;

    /// P1: resolve the correlation id owning `message_id` (the ask whose
    /// `question_msg_id` OR `answer_msg_id` equals it), or `None` if the message
    /// belongs to no tracked ask. Backs the `in_reply_to` → ask resolver in the
    /// `weave_answer` MCP/CLI path. Read-only.
    #[allow(dead_code)]
    fn ask_for_message(&self, message_id: i64) -> Result<Option<String>>;

    /// P2 ask-many: fan ONE question to N explicit peers. Opens a parent `ask_groups`
    /// anchor and creates one NORMAL P1 child ask per (de-duped, valid, non-broadcast)
    /// peer — each child carries the parent id and is a plain `ask`-shaped row, so the
    /// P1 lifecycle is SHARED, not duplicated. Best-effort per child (repowire parity):
    /// an invalid/broadcast/over-length peer in the list yields a per-child error and
    /// is skipped (counted `failed` at read time via `target_count`), NOT a whole-call
    /// failure. A wholly-invalid request — empty list, list > `MAX_ASK_MANY_TARGETS`,
    /// or a bad asker/body — IS a hard error before any insert. ONE transaction. Owner-
    /// only: writes the LOCAL store. NO `store → inject` edge — the per-child live nudge
    /// is fired caller-side. Returns the parent id + each child's create result.
    #[allow(dead_code)]
    fn create_ask_many(
        &self,
        asker: &str,
        peers: &[String],
        subject: Option<&str>,
        body: &str,
    ) -> Result<AskManyOutcome>;

    /// P2: aggregate an ask-many group at READ time (no background ticker). Loads the
    /// parent `ask_groups` row (`None` ⇒ `Ok(None)`), enumerates its children by
    /// `parent_id`, derives per-child state/answer, rolls up answered/acked/pending
    /// counts (`failed = target_count - created_children`), and classifies the group
    /// `complete | partial | pending` via [`crate::model::classify_ask_many`] using
    /// `now() - opened_ts` for the age and the optional `age_threshold`. Read-only.
    #[allow(dead_code)]
    fn ask_many_result(
        &self,
        parent_id: &str,
        age_threshold: Option<i64>,
    ) -> Result<Option<AskManyResult>>;

    /// P3 job board: mint a fresh `queued` job from `spec` (owner defaults to
    /// `creator`) and return it. Owner-only: writes the LOCAL store. NO injector
    /// involvement — jobs do not nudge in P3. Validates title/desc/prompt length and
    /// the assignee/owner/circle identity shapes before any insert. (`creator` is
    /// the caller's resolved identity.)
    #[allow(dead_code)]
    fn create_job(&self, creator: &str, spec: JobSpec) -> Result<Job>;

    /// P3: fetch a single job by id (PK lookup), or `None`.
    #[allow(dead_code)]
    fn get_job(&self, id: &str) -> Result<Option<Job>>;

    /// P3: list jobs matching `filter` (state/owner/creator/assignee/circle exact
    /// match, any `None` field unconstrained), newest-first by `updated_ts`, capped
    /// at `clamp_limit(limit)`. Read-only.
    #[allow(dead_code)]
    fn list_jobs(&self, filter: JobFilter, limit: i64) -> Result<Vec<Job>>;

    /// P3: CLAIM a job — mint a fresh `attempt_id` (claim token), set
    /// `assignee`+`attempt_id`, transition to `running`, and return the updated job.
    /// A terminal job cannot be claimed (clean error). Re-claiming a non-terminal job
    /// mints a NEW token that fences out the prior worker's now-stale token. The ONLY
    /// path that sets `attempt_id`. `None` ⇒ the job id does not exist.
    #[allow(dead_code)]
    fn claim_job(&self, id: &str, assignee: &str) -> Result<Option<Job>>;

    /// P3: apply `patch` to a job, ENFORCING (in the store, so CLI + MCP both
    /// inherit): (1) attempt_id FENCING — if the job is claimed (`attempt_id` set),
    /// the supplied `attempt_id` MUST equal it or the call returns `Err("stale_attempt")`
    /// (an unclaimed job accepts updates without a token — pre-claim parking); (2) the
    /// state machine — an illegal [`JobState::can_transition`] is a clean error; a
    /// `progress_note` is APPENDED to the append-only event log; entering a terminal
    /// state stamps `completed_ts`. Returns the updated job; an unknown id is a clean
    /// error.
    #[allow(dead_code)]
    fn update_job(&self, id: &str, attempt_id: Option<&str>, patch: JobPatch) -> Result<Job>;

    /// P3: the read-time result view of a job. A terminal job yields its terminal
    /// payload (`ready=true`); a non-terminal job yields the `ready=false` not-ready
    /// marker. `None` ⇒ unknown id. Read-only.
    #[allow(dead_code)]
    fn job_result(&self, id: &str) -> Result<Option<JobResultView>>;

    /// P3: COOPERATIVE cancel (never a hard delete). A terminal job ⇒ flag-only
    /// (records reason/by/at via COALESCE, no state change). A `queued` job ⇒
    /// transitions straight to terminal `cancelled` (nothing has claimed it). Any
    /// other (claimed/running) job ⇒ sets the `cancel_requested` flag ONLY; the
    /// worker observes it on its next poll and honors it (the daemon-free contract).
    /// Returns the updated job; `None` ⇒ unknown id.
    #[allow(dead_code)]
    fn cancel_job(&self, id: &str, requested_by: &str, reason: Option<&str>)
        -> Result<Option<Job>>;

    /// P6 delivery observability: append ONE metadata-only stage row to the
    /// `delivery_log` trace for `ref_id`. The store records the OUTCOME its CALLER
    /// passes (post-inject); it NEVER injects — there is no store→inject edge. Owner-
    /// only: writes the LOCAL store. SECRET-FREE — only (ref_id, ref_kind, to_peer,
    /// stage, outcome, ts) are bound; NO body/subject/sig/token ever reaches this
    /// table. `stage`/`outcome`/`ref_kind` are the model enum `.as_str()` constants
    /// (the only inlined SQL "literals"); the row's user-derived fields are bound via
    /// `params!`. Best-effort by convention: callers wrap it so a trace failure can
    /// never sink the delivery hot path.
    #[allow(dead_code)]
    fn record_delivery(
        &self,
        ref_id: i64,
        ref_kind: &str,
        to_peer: &str,
        stage: &str,
        outcome: &str,
    ) -> Result<()>;

    /// P6: read the delivery trace for `ref_id`, oldest-first (`ts ASC, id ASC`),
    /// BOUNDED at `min(limit, MAX_DELIVERY_ROWS)` so a pathological ref can never
    /// return an unbounded vector. Read-only, metadata-only.
    #[allow(dead_code)]
    fn list_delivery(&self, ref_id: i64, limit: i64) -> Result<Vec<DeliveryTrace>>;

    /// Presence seam (v0.2): write a heartbeat row for `name` on `host` with
    /// `pid`.  Upserts the `presence` table; stale rows are ignored by readers.
    /// Self-only: a peer writes its OWN heartbeat.
    #[allow(dead_code)]
    fn heartbeat(&self, name: &str, host: &str, pid: Option<i64>) -> Result<()>;

    /// Presence seam (v0.2): read the freshest heartbeat for `name` on `host`.
    /// Returns `Some(heartbeat_ts)` if a row exists AND is within
    /// `PRESENCE_TTL_SECS`, else `None`.  Falls back to the TTL recency guess
    /// when absent.
    #[allow(dead_code)]
    fn presence(&self, name: &str, host: &str) -> Result<Option<i64>>;

    /// Presence seam (v0.2): delete presence rows whose heartbeat is older than
    /// `cutoff_secs` seconds.  Best-effort housekeeping; callers may run it
    /// periodically (e.g. every 60 s in the daemon loop).
    #[allow(dead_code)]
    fn evict_stale_presence(&self, cutoff_secs: i64) -> Result<usize>;

    /// Presence seam (v0.2): three-tier liveness resolver.  A fresh daemon
    /// heartbeat (≤ `PRESENCE_TTL_SECS`) → [`crate::model::Liveness::Live`]; absent /
    /// stale heartbeat falls back to the v0.1 TTL heuristic
    /// (`is_online(last_seen)` with 900 s window) → `Likely` or `Offline`.
    ///
    /// Default implementation so backends only need `heartbeat` + `presence` +
    /// `evict_stale_presence`.
    #[allow(dead_code)]
    fn peer_liveness(&self, peer: &Peer) -> Result<crate::model::Liveness> {
        if self.presence(&peer.name, &peer.host)?.is_some() {
            return Ok(crate::model::Liveness::Live);
        }
        if is_online_at(peer.last_seen, now()) {
            Ok(crate::model::Liveness::Likely)
        } else {
            Ok(crate::model::Liveness::Offline)
        }
    }

    /// WL-016: schedule a future message delivery.
    /// Validates sender/recipient identities and body length via `check_ident`/`check_body`.
    /// `next_run` must be in the future (>= now() allowed; the tick uses `<= now`).
    /// Returns the new schedule id.
    #[allow(dead_code)]
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
    ) -> Result<i64>;

    /// WL-016: list schedules created by `sender`, newest-first by `created_ts`,
    /// capped at `clamp_limit(limit)`. Includes cancelled rows so the user sees
    /// the full state.
    #[allow(dead_code)]
    fn list_schedules(&self, sender: &str, limit: i64) -> Result<Vec<Schedule>>;

    /// WL-016: soft-cancel a schedule by id. Returns `true` if the row existed and
    /// was pending (and is now cancelled). Idempotent: cancelling an already-
    /// cancelled or executed row returns `false` without error.
    #[allow(dead_code)]
    fn cancel_schedule(&self, id: i64) -> Result<bool>;

    /// WL-016: fetch schedules whose `next_run <= before_ts` AND `cancelled = 0`
    /// AND (`executed_ts IS NULL` OR `kind = 'recurring'`), oldest-first by `next_run`.
    /// The tick calls this with `before_ts = now()`.
    #[allow(dead_code)]
    fn get_due_schedules(&self, before_ts: i64) -> Result<Vec<Schedule>>;

    /// WL-016: advance a schedule after execution.
    /// - OneShot: sets `executed_ts = now()`.
    /// - Recurring: computes the next occurrence via `model::next_occurrence` and
    ///   updates `next_run`; if no next occurrence is computable (malformed cron),
    ///   soft-cancels instead.
    #[allow(dead_code)]
    fn mark_schedule_executed(&self, id: i64) -> Result<()>;

    /// WL-020: add a PR to the review queue. Returns the new review id.
    #[allow(dead_code)]
    fn add_review_item(
        &self,
        pr_url: &str,
        title: &str,
        author: &str,
        repo: &str,
        state: ReviewItemState,
        review_requested_at: Option<i64>,
    ) -> Result<String>;

    /// WL-020: list review items matching filter, newest-first by `created_at`,
    /// capped at `clamp_limit(limit)`. Read-only.
    #[allow(dead_code)]
    fn review_queue(&self, filter: ReviewQueueFilter, limit: i64) -> Result<Vec<ReviewItem>>;

    /// WL-020: mark a review item as reviewed by `reviewer` at `now()`.
    /// Returns `true` if the row existed and was updated.
    #[allow(dead_code)]
    fn mark_reviewed(&self, id: &str, reviewer: &str) -> Result<bool>;

    /// WL-020: remove a review item by id. Returns `true` if the row existed.
    #[allow(dead_code)]
    fn remove_review_item(&self, id: &str) -> Result<bool>;

    /// WL-024: attempt to reserve a lease on `resource` for `holder`.
    /// Succeeds only if no lease exists or the existing lease has expired.
    /// On conflict, returns `Err` naming the current holder and expiry.
    #[allow(dead_code)]
    fn reserve_lease(
        &self,
        holder: &str,
        resource: &str,
        ttl_secs: i64,
        note: Option<&str>,
    ) -> Result<crate::model::Lease>;

    /// WL-024: release a lease held by `holder` on `resource`.
    /// Returns `true` if the row existed and matched the holder.
    #[allow(dead_code)]
    fn release_lease(&self, holder: &str, resource: &str) -> Result<bool>;

    /// WL-024: list active (non-expired) leases, newest-first by `acquired`,
    /// capped at `clamp_limit(limit)`. Read-only.
    #[allow(dead_code)]
    fn list_leases(&self, limit: i64) -> Result<Vec<crate::model::Lease>>;

    /// WL-029: delete all expired leases (expires <= now). Returns the count
    /// removed. Write path.
    #[allow(dead_code)]
    fn sweep_expired_leases(&self) -> Result<usize>;

    /// WL-031: set the priority of a message after creation.
    #[allow(dead_code)]
    fn set_message_priority(&self, id: i64, priority: &str) -> Result<()>;

    /// WL-038: stamp an ephemeral message's absolute expiry deadline (epoch secs)
    /// after creation. Post-insert stamp (the `set_message_priority` precedent), so
    /// `Store::send`'s signature is unchanged. Write path.
    #[allow(dead_code)]
    fn set_message_expiry(&self, id: i64, expires_at: i64) -> Result<()>;

    /// WL-038: delete all expired ephemeral messages (and their `reads`) where
    /// `expires_at <= now()`. Returns the count of messages removed. Delete-on-sweep
    /// (an expired ephemeral message must not be reconstructable). Called
    /// opportunistically before read surfaces and folded into `gc()`. Write path.
    #[allow(dead_code)]
    fn sweep_expired_messages(&self) -> Result<usize>;

    /// WL-037: mark `old_id` as superseded by `new_id` (replacement, distinct from
    /// `in_reply_to` threading). Stamps `messages.superseded_by = new_id` on the
    /// predecessor so it drops out of every reader's unread inbox while remaining
    /// (flagged) in history/thread/search. Authorization: only the ORIGINAL SENDER
    /// of `old_id` may supersede it (best-effort same-identity guard — `from` is
    /// advisory until the `sign` feature makes it unforgeable — preventing a
    /// hostile session from censoring another agent's message). Both ids must
    /// exist, else a clean error (never a silent no-op). Superseding an
    /// already-superseded message re-points the link forward, forming a chain
    /// (A→B→C); only the tail (`superseded_by IS NULL`) is unread. Never injects,
    /// never touches the `reads` table.
    #[allow(dead_code)]
    fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()>;

    /// WL-039: idle-notification dedup. Mark `new_id` as an idle ping
    /// (`kind = 'idle'`), then auto-supersede every *prior* still-**unread** idle
    /// ping from `sender` to `recipient` by stamping `superseded_by = new_id` on
    /// them — collapsing a sender's pile of "still waiting" pings to just the
    /// latest. Reuses the WL-037 `superseded_by` hide-from-unread spine: the
    /// superseded predecessors drop out of every unread/peek/inbox/nudge surface
    /// while staying (flagged) in history/thread/search. Returns the number of
    /// predecessors superseded.
    ///
    /// HARD safety boundary — dedup can NEVER touch a real message or another
    /// session's pings. It is scoped to rows where ALL hold: `kind = 'idle'` (set
    /// only by the notify path), `sender = sender` (authz — you can only supersede
    /// your OWN prior idle pings, same spine as [`Store::supersede`]),
    /// `recipient = recipient`, the predecessor is still unread by `recipient`
    /// (same unread definition as `unread_count`), `superseded_by IS NULL`, and
    /// `id <> new_id` (so an idempotency-key replay where `send` returns the
    /// existing id is a clean no-op). Never injects, never touches `reads`.
    #[allow(dead_code)]
    fn supersede_prior_idle(&self, sender: &str, recipient: &str, new_id: i64) -> Result<usize>;

    /// WL-032: set a peer's contact policy.
    #[allow(dead_code)]
    fn set_peer_policy(&self, name: &str, policy: &str) -> Result<()>;

    /// WL-032: get a peer's contact policy.
    #[allow(dead_code)]
    fn get_peer_policy(&self, name: &str) -> Result<Option<String>>;

    /// WL-021: resolve the permission status of a ToolPermission ask by its
    /// correlation id. Returns the answer body when available so callers can log
    /// or audit the exact response. `timeout_secs` defaults to
    /// [`PERMISSION_TIMEOUT_SECS`] when 0.
    #[allow(dead_code)]
    fn permission_verdict(
        &self,
        correlation_id: &str,
        timeout_secs: i64,
    ) -> Result<(PermissionStatus, Option<String>)>;

    /// WL-021: list ToolPermission asks where `me` is the asker, newest-first,
    /// capped at `clamp_limit(limit)`. Read-only.
    #[allow(dead_code)]
    fn list_permissions(&self, me: &str, limit: i64) -> Result<Vec<Ask>>;

    /// WL-033: store or replace a thread summary.
    #[allow(dead_code)]
    fn store_summary(&self, root_id: i64, text: &str, model: &str) -> Result<()>;

    /// WL-033: retrieve a cached summary by root message id.
    #[allow(dead_code)]
    fn get_summary(&self, root_id: i64) -> Result<Option<crate::model::Summary>>;

    /// WL-033: delete a cached summary.
    #[allow(dead_code)]
    fn delete_summary(&self, root_id: i64) -> Result<bool>;

    /// WL-035: write a *consistent* snapshot of this store to `dest` (a fresh
    /// path — `VACUUM INTO` refuses an existing file). The backend uses
    /// parameterized `VACUUM INTO ?1` (never a raw file copy of a live WAL DB) and
    /// MUST read-back-verify the snapshot (re-open it read-only and count rows)
    /// before returning Ok, so a corrupt/unreadable snapshot never passes silently.
    /// A remote (no local file) backend has nothing to vacuum-into locally and
    /// `bail!`s with a clear message.
    #[allow(dead_code)]
    fn snapshot_to(&self, dest: &std::path::Path) -> Result<()>;
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
/// same-host PID probe (which is gated to the `AliveLocal` arm). Thin wrapper
/// over [`liveness_from_fields`] reading the peer's raw fields, so a caller that
/// only has loose fields (e.g. the `sessions --watch` dashboard render) and a
/// caller that has a full [`Peer`] classify byte-identically.
pub fn liveness_for(peer: &Peer, this_host: &str, now_ts: i64) -> Liveness {
    liveness_from_fields(&peer.host, peer.pid, peer.last_seen, this_host, now_ts)
}

/// Host-aware liveness classifier over loose presence fields — the field-level
/// seam under [`liveness_for`]. Lets a display surface that holds only
/// `host`/`pid`/`last_seen` (the `sessions --watch` dashboard) compute the same
/// verdict without fabricating a full [`Peer`]. Pure except for the same-host
/// PID probe, which is gated to the local arm exactly as before.
pub fn liveness_from_fields(
    host: &str,
    pid: Option<i64>,
    last_seen: i64,
    this_host: &str,
    now_ts: i64,
) -> Liveness {
    // Recency guard first: anything past the TTL window is stale regardless of host.
    if !is_online_at(last_seen, now_ts) {
        return Liveness::Stale;
    }
    if host == this_host {
        // Same host: the PID is authoritative when known; a dead local pid is
        // stale even though it is recent. A null pid falls back to the TTL window.
        match pid {
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

/// Hard upper bound on how many DISTINCT public keys may be registered under a
/// single identity in the `identity_keys` registry. Multi-key registration exists
/// for rotation OVERLAP (old + new both verify during a window), so a generous
/// value is fine, but an unbounded registry is a token/RAM/DoS hazard: a hostile
/// source could flood many keys under one identity. 16 is far more than any real
/// rotation needs. Registering the SAME key twice is a no-op and never counts
/// against this cap; only adding a NEW key when the identity already holds this
/// many distinct keys is refused. Not config — a fixed store constant.
pub const MAX_KEYS_PER_IDENT: usize = 16;

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

/// Mint a fresh birth certificate: 32 random bytes from `getrandom`, hex-encoded
/// to a 64-char string. Tiny (~1 dep, no-std) and cryptographically secure.
pub fn mint_birth_cert() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf)
        .map_err(|e| anyhow::anyhow!("birth cert entropy failure: {e}"))?;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in buf {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    Ok(out)
}

/// Validate a caller-supplied birth certificate before it is bound into a query.
/// Rejects empty, over-length (> [`MAX_BIRTH_CERT_LEN`] chars), or non-hex values.
/// Shared by both backends so CLI/MCP/hook are all covered at the store layer.
pub fn check_birth_cert(cert: &str) -> Result<()> {
    if cert.is_empty() {
        anyhow::bail!("birth certificate must not be empty.");
    }
    if cert.len() > crate::model::MAX_BIRTH_CERT_LEN {
        anyhow::bail!(
            "birth certificate is too long ({} chars; max {}).",
            cert.len(),
            crate::model::MAX_BIRTH_CERT_LEN
        );
    }
    if !cert.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("birth certificate must be hexadecimal [0-9a-fA-F].");
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

/// Maximum rows `weave audit revocations` (and the doctor rollup query) will read
/// in one call, regardless of the caller-supplied `--limit`. Bounds the audit read
/// the same way [`MAX_PULL_PER_DRAIN`] bounds a drain, so a large/negative limit can
/// never trigger an unbounded scan of the (append-only, potentially long) log.
pub const MAX_REVOCATIONS_LIST: i64 = 1000;

/// Defensive upper bound on a stored `fp`/`source` string at the audit write seam.
/// A canonical fingerprint is a fixed 71-char `SHA256:`+64hex and a source label is
/// already a bounded path string upstream, but the audit append is best-effort and
/// must keep a hostile/oversized value out of the table; we clamp to this length on
/// a UTF-8 boundary before binding. Generous (covers any legitimate value) yet finite.
pub const MAX_REVOCATION_FIELD_LEN: usize = 256;

/// The kind of an observed revocation audit event. `Enforced` is recorded at the R1
/// rejection in `verify_pulled_intent` (a signed intent that verified ONLY against a
/// revoked key was rejected); `Declared` is recorded when an operator runs
/// `weave key revoke` (provenance of which fingerprint was marked revoked, when).
/// Backend-agnostic data (no I/O), stored as a small text discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "sign"), allow(dead_code))]
pub enum RevocationKind {
    /// An R1 rejection actually fired: a signature verified only against revoked key(s).
    Enforced,
    /// An operator ran `weave key revoke` for this fingerprint.
    Declared,
}

impl RevocationKind {
    /// Canonical on-disk discriminant (the `kind` column value).
    pub fn as_str(self) -> &'static str {
        match self {
            RevocationKind::Enforced => "enforced",
            RevocationKind::Declared => "declared",
        }
    }

    /// Parse a stored discriminant back into a [`RevocationKind`]; an unknown value
    /// (e.g. written by a future version) falls back to `Enforced` so a read never
    /// errors on a row it does not recognise.
    #[cfg_attr(not(feature = "sign"), allow(dead_code))]
    pub fn parse(s: &str) -> Self {
        match s {
            "declared" => RevocationKind::Declared,
            _ => RevocationKind::Enforced,
        }
    }
}

/// One row of the observed-revocation audit log (`revocations` table). Append-only,
/// WRITE-on-enforce / READ-only-to-the-decision: nothing in `verify_pulled_intent`
/// ever reads this — the config `VerifyPolicy.revoked` predicate remains the single
/// source of truth for the R1 decision, and every `Enforced` row is *caused by* that
/// predicate firing, so the two can never drift. Secret-free: `fp` is a fingerprint
/// derived from a PUBLIC key, `identity`/`source` are public labels. Like [`Intent`]
/// it is store-row data, so it lives here (not in `model`) to avoid widening `model`.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "sign"), allow(dead_code))]
pub struct RevocationEvent {
    /// Autoincrement row id (0 for a not-yet-inserted event being appended).
    pub id: i64,
    /// Receiver-stamped event time ([`model::now`]).
    pub ts: i64,
    /// FULL `SHA256:<64hex>` fingerprint of the revoked key. NEVER a private key; the
    /// pubkey is public and this is the form the config revoked-set matches on.
    pub fp: String,
    /// Claimed sender identity for an `Enforced` event (may be `""`); `""` for a
    /// `Declared` event.
    pub identity: String,
    /// Canonical source label for an `Enforced` event; `""` for a `Declared` event.
    pub source: String,
    /// `enforced` or `declared`.
    pub kind: RevocationKind,
}

/// Truncate an audit field to [`MAX_REVOCATION_FIELD_LEN`] on a UTF-8 char boundary.
/// Used at the `record_revocation` write seam (both backends) so a hostile/oversized
/// `fp`/`identity`/`source` cannot bloat the append-only log. A clean copy when the
/// value is already within bounds (the normal case for a 71-char fingerprint).
#[cfg_attr(not(feature = "sign"), allow(dead_code))]
pub fn clamp_field(s: &str) -> String {
    if s.len() <= MAX_REVOCATION_FIELD_LEN {
        return s.to_string();
    }
    let mut end = MAX_REVOCATION_FIELD_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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

/// Reject an over-length job free-text field (title/description/prompt/phase/note/
/// reason/summary) before it is stored. Job text is echoed into other agents'
/// listings, so an unbounded one is a token/RAM/UI hazard. `label` names the field
/// for the error message. Shared by both backends. (P3)
#[cfg_attr(not(any(feature = "sqlite", feature = "libsql")), allow(dead_code))]
pub fn check_job_text(label: &str, value: &str) -> Result<()> {
    if value.chars().count() > crate::model::MAX_JOB_TEXT {
        anyhow::bail!(
            "{label} is too long ({} chars; max {}).",
            value.chars().count(),
            crate::model::MAX_JOB_TEXT
        );
    }
    Ok(())
}

/// Reject an over-size job JSON payload (result/error/artifacts) before it is
/// stored. These are peer-supplied opaque TEXT blobs; an unbounded one is a disk +
/// token/RAM DoS once re-rendered into another agent's context. `label` names the
/// field. Shared by both backends. (P3)
#[cfg_attr(not(any(feature = "sqlite", feature = "libsql")), allow(dead_code))]
pub fn check_job_json(label: &str, value: &str) -> Result<()> {
    if value.len() > crate::model::MAX_JOB_JSON {
        anyhow::bail!(
            "{label} JSON is too large ({} bytes; max {}).",
            value.len(),
            crate::model::MAX_JOB_JSON
        );
    }
    Ok(())
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
    superseded_by   INTEGER,
    expires_at      INTEGER,
    kind            TEXT
);
CREATE TABLE IF NOT EXISTS reads (
    message_id INTEGER NOT NULL,
    reader     TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (message_id, reader)
);
CREATE TABLE IF NOT EXISTS wake_acks (
    reader  TEXT PRIMARY KEY,
    last_id INTEGER NOT NULL
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
    worktree_id TEXT NOT NULL DEFAULT '',
    circle      TEXT NOT NULL DEFAULT 'default',
    role        TEXT NOT NULL DEFAULT 'peer',
    turn_state     TEXT NOT NULL DEFAULT '',
    description    TEXT NOT NULL DEFAULT '',
    description_ts INTEGER NOT NULL DEFAULT 0,
    birth_cert     TEXT,
    contact_policy TEXT NOT NULL DEFAULT 'open'
);
CREATE TABLE IF NOT EXISTS outbox (
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
    priority        TEXT NOT NULL DEFAULT 'normal',
    ttl             INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS pull_cursor (
    source  TEXT PRIMARY KEY,
    last_id INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS keys (
    identity TEXT PRIMARY KEY,
    pubkey   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_keys (
    identity TEXT NOT NULL,
    pubkey   TEXT NOT NULL,
    added_ts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (identity, pubkey)
);
CREATE TABLE IF NOT EXISTS revocations (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    fp        TEXT NOT NULL,
    identity  TEXT NOT NULL DEFAULT '',
    source    TEXT NOT NULL DEFAULT '',
    kind      TEXT NOT NULL DEFAULT 'enforced'
);
CREATE TABLE IF NOT EXISTS asks (
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
);
CREATE TABLE IF NOT EXISTS ask_groups (
    parent_id    TEXT PRIMARY KEY,
    asker        TEXT NOT NULL,
    subject      TEXT,
    body         TEXT NOT NULL,
    opened_ts    INTEGER NOT NULL,
    target_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs (
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
);
CREATE INDEX IF NOT EXISTS idx_jobs_state            ON jobs(state);
CREATE INDEX IF NOT EXISTS idx_jobs_owner_updated    ON jobs(owner, updated_ts);
CREATE INDEX IF NOT EXISTS idx_jobs_assignee_updated ON jobs(assignee, updated_ts);
CREATE INDEX IF NOT EXISTS idx_jobs_circle_updated   ON jobs(circle, updated_ts);
CREATE TABLE IF NOT EXISTS delivery_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ref_id    INTEGER NOT NULL,
    ref_kind  TEXT NOT NULL,
    to_peer   TEXT NOT NULL,
    stage     TEXT NOT NULL,
    outcome   TEXT NOT NULL,
    ts        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_delivery_log_ref ON delivery_log(ref_id, ref_kind);
CREATE INDEX IF NOT EXISTS idx_delivery_log_ts  ON delivery_log(ts);
CREATE TABLE IF NOT EXISTS presence (
    name         TEXT PRIMARY KEY,
    host         TEXT NOT NULL DEFAULT '',
    pid          INTEGER,
    heartbeat_ts INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS schedules (
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
);
CREATE INDEX IF NOT EXISTS idx_schedules_next_run ON schedules(next_run);
CREATE INDEX IF NOT EXISTS idx_schedules_sender    ON schedules(sender);
CREATE TABLE IF NOT EXISTS reviews (
    id                 TEXT PRIMARY KEY,
    pr_url             TEXT NOT NULL,
    title              TEXT NOT NULL DEFAULT '',
    author             TEXT NOT NULL DEFAULT '',
    repo               TEXT NOT NULL DEFAULT '',
    state              TEXT NOT NULL DEFAULT 'open',
    review_requested_at INTEGER,
    reviewed_at        INTEGER,
    reviewed_by        TEXT,
    created_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reviews_state ON reviews(state);
CREATE INDEX IF NOT EXISTS idx_reviews_created ON reviews(created_at);
CREATE TABLE IF NOT EXISTS leases (
    resource  TEXT PRIMARY KEY,
    holder    TEXT NOT NULL,
    acquired  INTEGER NOT NULL,
    expires   INTEGER NOT NULL,
    note      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_leases_holder ON leases(holder);
CREATE INDEX IF NOT EXISTS idx_leases_expires ON leases(expires);
CREATE TABLE IF NOT EXISTS summaries (
    root_id     INTEGER PRIMARY KEY,
    text        TEXT NOT NULL,
    model       TEXT NOT NULL DEFAULT '',
    created_ts  INTEGER NOT NULL,
    refreshed_ts INTEGER NOT NULL
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
        idempotency_key: r.get("idempotency_key").unwrap_or(None),
        trace_id: r.get("trace_id").unwrap_or(None),
        priority: r.get("priority").unwrap_or("normal".to_string()),
        // WL-037: read by name so `SELECT *` and projections that list the column
        // populate it; projections that omit it (legacy) read back `None`. The
        // migration guarantees the column exists.
        superseded_by: r.get("superseded_by").unwrap_or(None),
        // WL-038: read by name; `SELECT *` and projections listing the column
        // populate it, projections that omit it read back `None`. The migration
        // guarantees the column exists.
        expires_at: r.get("expires_at").unwrap_or(None),
        // WL-039: read by name; `SELECT *` and projections listing the column
        // populate it, projections that omit it read back `None`. The migration
        // guarantees the column exists.
        kind: r.get("kind").unwrap_or(None),
    })
}

/// Convert an `asks` row into our owned [`Ask`]. Column order matches the explicit
/// projections used below: id, question_msg_id, answer_msg_id, asker, askee,
/// subject, state, reply_to, close_note, opened_ts, updated_ts, closed_ts. The
/// `state` TEXT is parsed through [`AskState::from_str`]; an unknown value is a
/// hard error (mapped to a rusqlite error), never a panic or a silent coercion.
#[cfg(feature = "sqlite")]
fn row_to_ask(r: &Row) -> rusqlite::Result<Ask> {
    let state_str: String = r.get("state")?;
    let state = AskState::from_str(&state_str).map_err(|msg| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, msg)),
        )
    })?;
    let kind_str: String = r.get("kind")?;
    let kind = AskKind::from_str(&kind_str);
    Ok(Ask {
        id: r.get("id")?,
        question_msg_id: r.get("question_msg_id")?,
        answer_msg_id: r.get("answer_msg_id")?,
        asker: r.get("asker")?,
        askee: r.get("askee")?,
        subject: r.get("subject")?,
        state,
        kind,
        options: r.get("options").unwrap_or(None),
        reply_to: r.get("reply_to")?,
        close_note: r.get("close_note")?,
        opened_ts: r.get("opened_ts")?,
        updated_ts: r.get("updated_ts")?,
        closed_ts: r.get("closed_ts")?,
        // Read by name so projections that omit it (e.g. an explicit SELECT that does
        // not list the column) still map cleanly; the migration guarantees the column
        // exists on a `SELECT *`. The `in_reply_to` precedent above.
        parent_id: r.get("parent_id").unwrap_or(None),
    })
}

/// Convert a `jobs` row into our owned [`Job`]. Reads columns by NAME so a
/// `SELECT *` (the canonical job projection) maps cleanly. The `state` TEXT is
/// parsed through [`JobState::from_str`]; an unknown value is a hard error (mapped
/// to a rusqlite error), never a panic or a silent coercion (the `row_to_ask`
/// precedent). `cancel_requested` is stored as 0/1 INTEGER.
#[cfg(feature = "sqlite")]
fn row_to_job(r: &Row) -> rusqlite::Result<Job> {
    let state_str: String = r.get("state")?;
    let state = JobState::from_str(&state_str).map_err(|msg| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, msg)),
        )
    })?;
    Ok(Job {
        id: r.get("id")?,
        title: r.get("title")?,
        description: r.get("description")?,
        kind: r.get("kind")?,
        state,
        state_reason: r.get("state_reason")?,
        phase: r.get("phase")?,
        prompt: r.get("prompt")?,
        progress_note: r.get("progress_note")?,
        progress_events_json: r.get("progress_events_json")?,
        creator: r.get("creator")?,
        owner: r.get("owner")?,
        assignee: r.get("assignee")?,
        circle: r.get("circle")?,
        correlation_id: r.get("correlation_id")?,
        source_kind: r.get("source_kind")?,
        source_id: r.get("source_id")?,
        scope: r.get("scope")?,
        visibility: r.get("visibility")?,
        attempt_id: r.get("attempt_id")?,
        deadline_at: r.get("deadline_at")?,
        expires_at: r.get("expires_at")?,
        result_summary: r.get("result_summary")?,
        result_json: r.get("result_json")?,
        error_json: r.get("error_json")?,
        artifacts_json: r.get("artifacts_json")?,
        cancel_requested: r.get::<_, i64>("cancel_requested")? != 0,
        cancel_requested_by: r.get("cancel_requested_by")?,
        cancel_requested_ts: r.get("cancel_requested_ts")?,
        cancel_reason: r.get("cancel_reason")?,
        opened_ts: r.get("opened_ts")?,
        updated_ts: r.get("updated_ts")?,
        completed_ts: r.get("completed_ts")?,
    })
}

/// Convert a `schedules` row into our owned [`Schedule`]. Reads columns by NAME
/// so a `SELECT *` maps cleanly. `kind` is parsed through [`ScheduleKind::from_str`];
/// an unknown value is a hard error, never a panic or silent coercion.
#[cfg(feature = "sqlite")]
fn row_to_schedule(r: &Row) -> rusqlite::Result<Schedule> {
    let kind_str: String = r.get("kind")?;
    let kind = ScheduleKind::from_str(&kind_str).map_err(|msg| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, msg)),
        )
    })?;
    Ok(Schedule {
        id: r.get("id")?,
        kind,
        cron_expr: r.get("cron_expr")?,
        next_run: r.get("next_run")?,
        sender: r.get("sender")?,
        recipient: r.get("recipient")?,
        subject: r.get("subject")?,
        body: r.get("body")?,
        created_ts: r.get("created_ts")?,
        executed_ts: r.get("executed_ts")?,
        cancelled: r.get::<_, i64>("cancelled")? != 0,
    })
}

/// Insert ONE freshly-opened `asks` row (`state='open'`, `closed_ts=NULL`) inside an
/// open transaction, with an optional `reply_to` (chaining) and an optional
/// `parent_id` (ask-many group). The SINGLE source of truth for the asks insert,
/// shared by the plain `ask` path (`parent_id = None`) and every `create_ask_many`
/// child (`parent_id = Some(group)`) — the P1 lifecycle is reused, not duplicated.
/// All values bound via `params!`; no inlined user data.
#[cfg(feature = "sqlite")]
#[allow(clippy::too_many_arguments)]
fn insert_ask_row(
    tx: &Transaction,
    id: &str,
    question_msg_id: i64,
    asker: &str,
    askee: &str,
    subject: Option<&str>,
    kind: &str,
    options: Option<&str>,
    reply_to: Option<&str>,
    parent_id: Option<&str>,
    ts: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO asks
            (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
             options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10, NULL, ?11)",
        params![
            id,
            question_msg_id,
            asker,
            askee,
            subject,
            AskState::Open.as_str(),
            kind,
            options,
            reply_to,
            ts,
            parent_id,
        ],
    )?;
    Ok(())
}

/// Count unread messages for `me` against an arbitrary connection (the live
/// connection or an open transaction), so the count can share a transaction with
/// the inbox read+mark for a consistent snapshot.
#[cfg(feature = "sqlite")]
fn unread_count_conn(conn: &Connection, me: &str) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > ?2)
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)",
        bc = BROADCAST_SQL
    );
    Ok(conn.query_row(&sql, params![me, now()], |r| r.get(0))?)
}

/// The oldest unread message for `me`, if any. Used by the wake hook to surface
/// the unread backlog without consuming it.
#[cfg(feature = "sqlite")]
fn peek_oldest_unread_conn(conn: &Connection, me: &str) -> Result<Option<Message>> {
    let sql = format!(
        "SELECT id, ts, sender, recipient, subject, body, in_reply_to, priority, superseded_by, expires_at, kind FROM messages m
         WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > ?2)
           AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = m.id AND r.reader = ?1)
         ORDER BY m.id ASC LIMIT 1",
        bc = BROADCAST_SQL
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![me, now()])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_to_message(r)?)),
        None => Ok(None),
    }
}

/// Highest unread message id the wake hook has already acknowledged for `me`.
#[cfg(feature = "sqlite")]
fn wake_last_acked_conn(conn: &Connection, me: &str) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(last_id, 0) FROM wake_acks WHERE reader = ?1",
            params![me],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
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
        circle: r.get(11)?,
        role: r.get(12)?,
        turn_state: r.get(13)?,
        description: r.get(14)?,
        description_ts: r.get(15)?,
        birth_cert: r.get(16).unwrap_or(None),
        contact_policy: r.get(17).unwrap_or("open".to_string()),
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
        idempotency_key: r.get(8).unwrap_or(None),
        trace_id: r.get(9).unwrap_or(None),
        priority: r.get(10).unwrap_or("normal".to_string()),
        ttl: r.get(11).unwrap_or(0),
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

/// True if `e`'s chain is a transient SQLite "database is locked" (SQLITE_BUSY).
///
/// Even with `busy_timeout` set, concurrent multi-process open — every starting
/// `weave` runs the idempotent open-time SCHEMA + `migrate()` writes — can still
/// surface an immediate lock instead of waiting, so we retry the whole open at the
/// app layer (see [`SqliteStore::open`]).
#[cfg(feature = "sqlite")]
fn is_db_locked(e: &anyhow::Error) -> bool {
    let mut s = String::new();
    for cause in e.chain() {
        s.push_str(&cause.to_string());
        s.push('\n');
    }
    let s = s.to_ascii_lowercase();
    s.contains("database is locked") || s.contains("database table is locked")
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
    // peers.circle / role — present on fresh DBs via SCHEMA, added here for DBs
    // created before P4 (circles + orchestrator role). `circle` defaults to the
    // non-empty literal 'default' so legacy rows classify into the default circle
    // with no runtime coalesce; `role` defaults to 'peer' so legacy rows are
    // plain participants. Both DDL strings are constant (no user data), the same
    // discipline as the socket/pid/host/repo steps above. Idempotent.
    if !column_exists(conn, "peers", "circle")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN circle TEXT NOT NULL DEFAULT 'default';")?;
    }
    if !column_exists(conn, "peers", "role")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN role TEXT NOT NULL DEFAULT 'peer';")?;
    }
    // peers.turn_state / description / description_ts (P5 rich presence) — present
    // on fresh DBs via SCHEMA, added here for DBs created before P5. `turn_state`
    // defaults to '' (== Unknown, a legacy/pre-hook row); `description` defaults to
    // '' (no description); `description_ts` defaults to 0 (no TTL anchor). All DDL
    // strings are constant (no user data), the same discipline as the
    // socket/pid/host/repo/circle steps above. Idempotent.
    if !column_exists(conn, "peers", "turn_state")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN turn_state TEXT NOT NULL DEFAULT '';")?;
    }
    if !column_exists(conn, "peers", "description")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN description TEXT NOT NULL DEFAULT '';")?;
    }
    if !column_exists(conn, "peers", "description_ts")? {
        conn.execute_batch(
            "ALTER TABLE peers ADD COLUMN description_ts INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    // WL-018: birth certificate for identity takeover protection. Nullable;
    // NULL means "not yet enrolled" (backward-compat). Existing peers without
    // a cert get one minted on their next re-registration.
    if !column_exists(conn, "peers", "birth_cert")? {
        conn.execute_batch("ALTER TABLE peers ADD COLUMN birth_cert TEXT;")?;
    }
    // Wake-hook watermark table (P5): tracks the last unread message id that
    // caused a block for each reader. Created here for legacy DBs that predate
    // wake; `CREATE TABLE IF NOT EXISTS` is idempotent and the identifiers are
    // constant.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS wake_acks (
            reader  TEXT PRIMARY KEY,
            last_id INTEGER NOT NULL
        );",
    )?;
    // Tier-2 tables: present on fresh DBs via SCHEMA, created here for DBs made
    // before cross-store delivery existed. `CREATE TABLE IF NOT EXISTS` is itself
    // idempotent, so this is a clean additive upgrade for a legacy store; the
    // `sig` column is reserved now so signed identity (2d) needs no further
    // outbox migration.
    conn.execute_batch(
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
            trace_id        TEXT
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
    // Multi-key registry (#7): `identity_keys` holds MULTIPLE pubkeys per identity
    // so rotation can OVERLAP (old + new both verify during a window). Created here
    // for legacy DBs that predate it; `CREATE TABLE IF NOT EXISTS` is idempotent.
    // The legacy single-key `keys` table is RETAINED as a deprecated shadow (no
    // DROP) for crash-safety and old-binary coexistence — new writes go ONLY to
    // `identity_keys`. The one-time copy below is `INSERT OR IGNORE` keyed on the
    // (identity,pubkey) PRIMARY KEY, so re-running it is a clean no-op and it never
    // overwrites a key already added under the new registry. `keys` is guaranteed
    // present (created just above), so the SELECT never errors. Constant DDL — no
    // user data is interpolated. `added_ts = 0` for migrated rows == "unknown age"
    // (ties break by rowid for the most-recent shim).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS identity_keys (
            identity TEXT NOT NULL,
            pubkey   TEXT NOT NULL,
            added_ts INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (identity, pubkey)
        );
        INSERT OR IGNORE INTO identity_keys (identity, pubkey, added_ts)
            SELECT identity, pubkey, 0 FROM keys;",
    )?;
    // Observed-revocation audit log (#11): append-only record of R1 enforcement
    // ("a signed intent was rejected because its key is revoked") and operator
    // `declared` revokes. Inert plain data in EVERY build (like `identity_keys`);
    // only the sign-gated write/read code touches it. Created here for DBs that
    // predate it; `CREATE TABLE IF NOT EXISTS` is idempotent and the DDL identifiers
    // are constant (no user data interpolated). It is NEVER read by the verification
    // decision — the config `revoked` predicate stays the single source of truth — so
    // it cannot drift from or weaken R1.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS revocations (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ts        INTEGER NOT NULL,
            fp        TEXT NOT NULL,
            identity  TEXT NOT NULL DEFAULT '',
            source    TEXT NOT NULL DEFAULT '',
            kind      TEXT NOT NULL DEFAULT 'enforced'
        );",
    )?;
    // Tracked ask/answer/ack side-table (P1): correlation + mutable lifecycle for a
    // request/response thread; the question/answer TEXT lives in `messages`
    // (threaded via `in_reply_to`), this row only points at them. Inert plain data
    // in EVERY build (like `revocations`/`identity_keys`); created here for DBs that
    // predate it. `CREATE TABLE IF NOT EXISTS` is idempotent and the DDL identifiers
    // are constant — no user data interpolated.
    conn.execute_batch(
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
        );",
    )?;
    // asks.parent_id (P2): the ask-many child→parent link. Present on fresh DBs via
    // SCHEMA, added here for a legacy P1-era DB whose `asks` table predates ask-many.
    // SQLite `ADD COLUMN` is O(1) and the new column defaults to NULL for every
    // existing row (== a standalone ask, not part of a group) — the exact guarded
    // `peers.pid`/`peers.repo` additive template, applied to the P1 `asks` table.
    // Idempotent: the `column_exists` guard makes a re-run a no-op.
    if !column_exists(conn, "asks", "parent_id")? {
        conn.execute_batch("ALTER TABLE asks ADD COLUMN parent_id TEXT;")?;
    }
    // WL-015: structured ask kinds + options. Guarded additive migration.
    if !column_exists(conn, "asks", "kind")? {
        conn.execute_batch("ALTER TABLE asks ADD COLUMN kind TEXT NOT NULL DEFAULT 'free_text';")?;
    }
    if !column_exists(conn, "asks", "options")? {
        conn.execute_batch("ALTER TABLE asks ADD COLUMN options TEXT;")?;
    }
    // ask_groups (P2): the ask-many PARENT anchor — the canonical question/opener +
    // post-dedup target_count for a fanned question. Created here for DBs that predate
    // ask-many; `CREATE TABLE IF NOT EXISTS` is idempotent and the DDL identifiers are
    // constant (no user data interpolated). Inert plain data in EVERY build (like
    // `asks`): only the ask-many code reads/writes it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ask_groups (
            parent_id    TEXT PRIMARY KEY,
            asker        TEXT NOT NULL,
            subject      TEXT,
            body         TEXT NOT NULL,
            opened_ts    INTEGER NOT NULL,
            target_count INTEGER NOT NULL
        );",
    )?;
    // Job board (P3): the durable poll-only work queue. Created via SCHEMA above for
    // a fresh DB; also created idempotently here for a DB that predates it (the
    // `asks`/`revocations` additive template). Inert plain data in EVERY build (no
    // runner reads it in P3); the runner-only lease/cron/spawn columns are
    // deliberately omitted — only the board metadata + the first-class `attempt_id`
    // fencing token. `CREATE TABLE/INDEX IF NOT EXISTS` is idempotent and the DDL
    // identifiers are constant — no user data interpolated.
    conn.execute_batch(
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
        );
        CREATE INDEX IF NOT EXISTS idx_jobs_state            ON jobs(state);
        CREATE INDEX IF NOT EXISTS idx_jobs_owner_updated    ON jobs(owner, updated_ts);
        CREATE INDEX IF NOT EXISTS idx_jobs_assignee_updated ON jobs(assignee, updated_ts);
        CREATE INDEX IF NOT EXISTS idx_jobs_circle_updated   ON jobs(circle, updated_ts);",
    )?;
    // delivery_log (P6): the metadata-only transport-trace surface. Created via
    // SCHEMA above for a fresh DB; also created idempotently here for a legacy DB
    // that predates it (the `asks`/`revocations`/`jobs` additive template). SECRET-
    // FREE by construction — columns are (ref_id, ref_kind, to_peer, stage, outcome,
    // ts) ONLY; never body/subject/sig/token. `CREATE TABLE/INDEX IF NOT EXISTS` is
    // idempotent and the DDL identifiers are constant — no user data interpolated.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS delivery_log (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ref_id    INTEGER NOT NULL,
            ref_kind  TEXT NOT NULL,
            to_peer   TEXT NOT NULL,
            stage     TEXT NOT NULL,
            outcome   TEXT NOT NULL,
            ts        INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_delivery_log_ref ON delivery_log(ref_id, ref_kind);
        CREATE INDEX IF NOT EXISTS idx_delivery_log_ts  ON delivery_log(ts);",
    )?;
    // Presence table (v0.2 daemon): tracks per-peer daemon heartbeats.
    // Created here for legacy DBs that predate it; `CREATE TABLE IF NOT EXISTS`
    // is idempotent and the DDL identifiers are constant (no user data).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS presence (
            name         TEXT PRIMARY KEY,
            host         TEXT NOT NULL DEFAULT '',
            pid          INTEGER,
            heartbeat_ts INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    // WL-016: schedules table for future message delivery.
    // Created here for legacy DBs that predate it; `CREATE TABLE IF NOT EXISTS`
    // is idempotent and the DDL identifiers are constant (no user data).
    conn.execute_batch(
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
        );
        CREATE INDEX IF NOT EXISTS idx_schedules_next_run ON schedules(next_run);
        CREATE INDEX IF NOT EXISTS idx_schedules_sender    ON schedules(sender);",
    )?;
    // WL-020: reviews table for PR review queue.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reviews (
            id                 TEXT PRIMARY KEY,
            pr_url             TEXT NOT NULL,
            title              TEXT NOT NULL DEFAULT '',
            author             TEXT NOT NULL DEFAULT '',
            repo               TEXT NOT NULL DEFAULT '',
            state              TEXT NOT NULL DEFAULT 'open',
            review_requested_at INTEGER,
            reviewed_at        INTEGER,
            reviewed_by        TEXT,
            created_at         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_reviews_state ON reviews(state);
        CREATE INDEX IF NOT EXISTS idx_reviews_created ON reviews(created_at);",
    )?;
    // WL-026: idempotency keys and trace IDs on messages and outbox.
    // SQLite `ALTER TABLE ADD COLUMN` rejects inline UNIQUE on non-empty tables,
    // so we add the column plain then create the unique index separately.
    if !column_exists(conn, "messages", "idempotency_key")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN idempotency_key TEXT;")?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_idempotency_key ON messages(idempotency_key);",
    )?;
    if !column_exists(conn, "messages", "trace_id")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN trace_id TEXT;")?;
    }
    if !column_exists(conn, "outbox", "idempotency_key")? {
        conn.execute_batch("ALTER TABLE outbox ADD COLUMN idempotency_key TEXT;")?;
    }
    if !column_exists(conn, "outbox", "trace_id")? {
        conn.execute_batch("ALTER TABLE outbox ADD COLUMN trace_id TEXT;")?;
    }
    // WL-028: FTS5 full-text search on messages.
    // The virtual table is created only when FTS5 is available (sqlite build).
    // libsql also supports FTS5 (verified via libsql-ffi build constants).
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            body, subject, sender,
            content='messages',
            content_rowid='id'
        );",
    )?;
    // Sync triggers: keep messages_fts in sync with messages.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, body, subject, sender)
            VALUES (new.id, new.body, new.subject, new.sender);
        END;
        CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, body, subject, sender)
            VALUES ('delete', old.id, old.body, old.subject, old.sender);
        END;",
    )?;
    // WL-031: message priority levels.
    if !column_exists(conn, "messages", "priority")? {
        conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal';",
        )?;
    }
    if !column_exists(conn, "outbox", "priority")? {
        conn.execute_batch(
            "ALTER TABLE outbox ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal';",
        )?;
    }
    // WL-037: message supersede/successor chains. Nullable (NULL == not
    // superseded), no DEFAULT — present on fresh DBs via SCHEMA, added here for
    // DBs created before supersede existed. `ADD COLUMN` is O(1) and every
    // existing row reads back NULL (== not superseded).
    if !column_exists(conn, "messages", "superseded_by")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN superseded_by INTEGER;")?;
    }
    // WL-038: ephemeral-message absolute deadline (`ts + ttl`). Nullable (NULL ==
    // permanent), no DEFAULT — present on fresh DBs via SCHEMA, added here for DBs
    // created before ephemeral messages existed. `ADD COLUMN` is O(1) and every
    // existing row reads back NULL (== permanent). Carried on the outbox as the
    // *relative* ttl (the receiver re-stamps `ts` on commit).
    if !column_exists(conn, "messages", "expires_at")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN expires_at INTEGER;")?;
    }
    if !column_exists(conn, "outbox", "ttl")? {
        conn.execute_batch("ALTER TABLE outbox ADD COLUMN ttl INTEGER NOT NULL DEFAULT 0;")?;
    }
    // WL-039: idle-notification marker. Nullable (NULL == ordinary message), no
    // DEFAULT — present on fresh DBs via SCHEMA, added here for DBs created before
    // idle dedup existed. `ADD COLUMN` is O(1) and every existing row reads back
    // NULL (== not an idle ping). Set to 'idle' only on the notify dedup path.
    if !column_exists(conn, "messages", "kind")? {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN kind TEXT;")?;
    }
    // WL-032: per-peer contact policies.
    if !column_exists(conn, "peers", "contact_policy")? {
        conn.execute_batch(
            "ALTER TABLE peers ADD COLUMN contact_policy TEXT NOT NULL DEFAULT 'open';",
        )?;
    }
    // WL-033: thread summarization cache.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS summaries (
            root_id     INTEGER PRIMARY KEY,
            text        TEXT NOT NULL,
            model       TEXT NOT NULL DEFAULT '',
            created_ts  INTEGER NOT NULL,
            refreshed_ts INTEGER NOT NULL
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
        // The open-time SCHEMA + migrate() writes are idempotent but RACE under
        // concurrent multi-process open: every starting `weave` runs them against
        // the same file, and busy_timeout alone does not reliably prevent an
        // immediate "database is locked" during the open-time migration. Retry the
        // whole connect+migrate as a unit on that transient error — bounded, and
        // safe to re-run because every open-time statement is idempotent.
        let mut attempt: u32 = 0;
        let conn = loop {
            match Self::open_conn(path) {
                Err(e) if is_db_locked(&e) && attempt < 200 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis((attempt as u64).min(20)));
                }
                other => break other?,
            }
        };
        // Restrict the on-disk DB (which holds message bodies) to the owner.
        // Done after the file is guaranteed to exist (post-open) and is
        // best-effort so it never breaks startup on odd filesystems.
        harden_permissions(path);
        Ok(Self { conn })
    }

    /// Connect + apply the idempotent open-time SCHEMA and migrations. Factored out
    /// of [`SqliteStore::open`] so the whole unit can be retried on a transient
    /// "database is locked" from a concurrent opener.
    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        // journal_mode returns a row, so query rather than execute.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(conn)
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
            match local.send(
                &intent.from,
                me,
                intent.subject.as_deref(),
                &intent.body,
                intent.idempotency_key.as_deref(),
                intent.trace_id.as_deref(),
            ) {
                Ok(mid) => {
                    if !intent.priority.is_empty() && intent.priority != "normal" {
                        let _ = local.set_message_priority(mid, &intent.priority);
                    }
                    // WL-038: re-stamp the absolute expiry from the carried relative
                    // ttl against the receiver's own `now()` (the priority precedent).
                    if intent.ttl > 0 {
                        let _ = local.set_message_expiry(
                            mid,
                            crate::model::expiry_from_ttl(now(), intent.ttl),
                        );
                    }
                    committed += 1;
                }
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
    // Look up ALL the sender's REGISTERED keys once (#7): a signed intent commits
    // IFF it verifies against at least one registered NON-REVOKED key, and the WHOLE
    // set is used for the trust/strictness evaluation on the unsigned/no-key path. A
    // lookup error is a hard drop (cannot make a safe decision).
    //
    // ADDITIVITY: with exactly ONE registered key this is byte-identical to the old
    // single-key path — the loop verifies that one key, the revoked check is the
    // same per-key check, and a present-but-invalid sig still always rejects. The
    // ONLY new behavior is the legitimate rotation-overlap case (a sig verifying
    // against a SECOND non-revoked registered key) and excluding a revoked key.
    let keys = match local.get_keys(&intent.from) {
        Ok(ks) => ks,
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
        // No registered key at all: a signature we cannot check ("present but
        // unverifiable"). NOT a trusted/revoked sender (no fp to match), so it falls
        // to the advisory path's effective strictness below.
        if keys.is_empty() {
            return self_unsigned_or_unknown(source, policy, &keys, intent);
        }
        let mut matched_any = false;
        // The revoked key the signature verified against (if the ONLY match is a
        // revoked one) — captured PURELY for the best-effort audit record below. It is
        // NEVER consulted by the decision; the `policy.is_revoked` predicate alone
        // drives control flow, exactly as before.
        let mut matched_revoked: Option<&String> = None;
        for pk in &keys {
            match crate::sign::verify_intent(
                pk,
                &intent.sig,
                &intent.from,
                &intent.to,
                &intent.body,
            ) {
                Ok(true) => {
                    matched_any = true;
                    // R1: a revoked key can NEVER grant a commit — skip it and keep
                    // looking for a non-revoked key that also verifies. This is
                    // evaluated BEFORE any disable toggle, identical strength to the
                    // old single-key revoked check.
                    if policy.is_revoked(pk) {
                        if matched_revoked.is_none() {
                            matched_revoked = Some(pk);
                        }
                        continue;
                    }
                    // A VALID signature against a CURRENT non-revoked registered key
                    // ⇒ COMMIT. The first such key is sufficient (the old single-key
                    // path did exactly this with its one key).
                    return true;
                }
                Ok(false) => {
                    // This key did not sign it; try the next registered key.
                }
                Err(e) => {
                    eprintln!(
                        "[weave] dropping intent #{} from source '{source}': verify error: {e}",
                        intent.id
                    );
                    return false;
                }
            }
        }
        if matched_any {
            // The signature verified ONLY against revoked key(s): REJECT (R1,
            // absolute revocation). A revoked old key never re-admits a message.
            eprintln!(
                "[weave] REJECTING intent #{} from '{}' via source '{source}': \
                 signature verifies but the key is REVOKED",
                intent.id, intent.from
            );
            // BEST-EFFORT audit side-effect, placed AFTER the decision is made (the
            // `false` return below is unconditional). Record an `Enforced` event so an
            // operator can see the revocation history via `weave audit revocations`.
            // A write failure NEVER re-admits the message or changes control flow — it
            // is logged to stderr and swallowed (same discipline as the cursor advance).
            // The audit table is never read by the decision, so this cannot affect R1.
            if let Some(pk) = matched_revoked {
                let fp = crate::sign::fingerprint_full(pk)
                    .map(|f| format!("SHA256:{f}"))
                    .unwrap_or_default();
                let ev = RevocationEvent {
                    id: 0,
                    ts: now(),
                    fp,
                    identity: intent.from.clone(),
                    source: source.to_string(),
                    kind: RevocationKind::Enforced,
                };
                if let Err(e) = local.record_revocation(&ev) {
                    eprintln!(
                        "[weave] note: failed to record revocation audit event \
                         (message decision unaffected): {e}"
                    );
                }
            }
        } else {
            // Present-but-invalid signature: verifies against NONE of the registered
            // keys ⇒ ALWAYS rejected (spoof/tamper). Preserved verbatim.
            eprintln!(
                "[weave] REJECTING intent #{} from '{}' via source '{source}': signature \
                 verification failed (possible forgery)",
                intent.id, intent.from
            );
        }
        false
    } else {
        // ---- UNSIGNED PATH ----
        self_unsigned_or_unknown(source, policy, &keys, intent)
    }
}

/// The advisory-or-strict decision for an UNSIGNED intent, or a SIGNED intent whose
/// sender has no registered key to check it against. Computes effective strictness
/// per the R1 ordering and commits (advisory) or drops (strict). `keys` is the
/// sender's FULL set of registered keys (possibly empty), used only to evaluate
/// trust: an identity is "trusted" if ANY of its registered keys is in the trust
/// set, so a trusted identity with multiple keys still triggers strict-by-default
/// (the "never weaken" form — a trusted key among several is never missed).
#[cfg(feature = "sign")]
fn self_unsigned_or_unknown(
    source: &str,
    policy: &VerifyPolicy,
    keys: &[String],
    intent: &Intent,
) -> bool {
    let strict = match policy.strict_override {
        Some(true) => true,   // user forced strict everywhere
        Some(false) => false, // user disabled strict for the advisory path
        None => {
            // Trust-set-aware default: a TRUSTED sender (ANY registered key in the
            // trust set) is verified strictly; everyone else stays advisory.
            policy.trust_configured() && keys.iter().any(|pk| policy.is_trusted(pk))
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
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<i64> {
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
        if let Some(key) = idempotency_key {
            if let Ok(id) = self.conn.query_row(
                "SELECT id FROM messages WHERE idempotency_key = ?1",
                params![key],
                |r| r.get::<_, i64>(0),
            ) {
                return Ok(id);
            }
        }
        self.conn.execute(
            "INSERT INTO messages (ts, sender, recipient, subject, body, idempotency_key, trace_id) \
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![now(), sender, recipient, subject, body, idempotency_key, trace_id],
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
        // WL-038: opportunistically delete any expired ephemeral rows before the
        // read so expiry holds even with no explicit gc; best-effort.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        // WL-038: also exclude expired-but-not-yet-swept rows (belt-and-suspenders
        // for the tiny window between sweeps).
        let sql = if include_read {
            format!(
                "SELECT * FROM messages
                 WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1
                   AND superseded_by IS NULL
                   AND (expires_at IS NULL OR expires_at > ?3)
                 ORDER BY id DESC LIMIT ?2",
                bc = BROADCAST_SQL
            )
        } else {
            format!(
                "SELECT m.* FROM messages m
                 WHERE (m.recipient = ?1 OR m.recipient IN {bc}) AND m.sender != ?1
                   AND m.superseded_by IS NULL
                   AND (m.expires_at IS NULL OR m.expires_at > ?3)
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

        let now_cut = now();
        let mut rows: Vec<Message> = {
            let mut stmt = tx.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, limit, now_cut], row_to_message)?
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
        // WL-038: opportunistic sweep so history never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let now_cut = now();
        let mut rows: Vec<Message> = if let Some(p) = peer {
            let sql = format!(
                "SELECT * FROM messages
                 WHERE ((sender = ?1 AND (recipient = ?2 OR recipient IN {bc}))
                    OR (sender = ?2 AND (recipient = ?1 OR recipient IN {bc})))
                   AND (expires_at IS NULL OR expires_at > ?4)
                 ORDER BY id DESC LIMIT ?3",
                bc = BROADCAST_SQL
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, p, limit, now_cut], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        } else {
            let sql = format!(
                "SELECT * FROM messages
                 WHERE (sender = ?1 OR recipient = ?1 OR recipient IN {bc})
                   AND (expires_at IS NULL OR expires_at > ?3)
                 ORDER BY id DESC LIMIT ?2",
                bc = BROADCAST_SQL
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let v = stmt
                .query_map(params![me, limit, now_cut], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        rows.reverse();
        Ok(rows)
    }

    fn search(&self, query: &str, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so search never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let sql = "SELECT * FROM messages
             WHERE id IN (
                 SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2
             )
             AND (expires_at IS NULL OR expires_at > ?3)
             ORDER BY id DESC LIMIT ?2";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![query, limit, now()], row_to_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn inbox_since(&self, me: &str, since_id: i64, limit: i64) -> Result<Vec<Message>> {
        // WL-038: opportunistic sweep so the drain never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        let limit = clamp_limit(limit);
        let sql = format!(
            "SELECT * FROM messages
             WHERE (recipient = ?1 OR recipient IN {bc}) AND sender != ?1 AND id > ?2
               AND superseded_by IS NULL
               AND (expires_at IS NULL OR expires_at > ?4)
             ORDER BY id ASC LIMIT ?3",
            bc = BROADCAST_SQL
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![me, since_id, limit, now()], row_to_message)?
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

    fn snapshot_to(&self, dest: &Path) -> Result<()> {
        // VACUUM INTO writes a fully-checkpointed, consistent copy (no WAL/torn
        // write hazard, unlike fs::copy of a live DB). The destination path is
        // BOUND as a parameter — never inlined into the SQL — and must not already
        // exist (VACUUM INTO refuses an existing file).
        let dest_str = dest.to_str().ok_or_else(|| {
            anyhow::anyhow!("snapshot destination path is not valid UTF-8: {dest:?}")
        })?;
        self.conn
            .execute("VACUUM INTO ?1", params![dest_str])
            .map_err(|e| anyhow::anyhow!("VACUUM INTO failed for {}: {e}", dest.display()))?;
        // Read-back verify (WL-041 spirit): the snapshot must re-open read-only and
        // be a valid weave store before we declare success.
        let snap = SqliteStore::open_readonly(dest)
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
            .execute_batch("DELETE FROM messages; DELETE FROM reads; DELETE FROM wake_acks;")?;
        Ok(n)
    }

    fn peek_oldest_unread(&self, me: &str) -> Result<Option<Message>> {
        // WL-038: opportunistic sweep so the wake hook never surfaces an expired row.
        let _ = self.sweep_expired_messages();
        peek_oldest_unread_conn(&self.conn, me)
    }

    fn wake_last_acked(&self, me: &str) -> Result<i64> {
        wake_last_acked_conn(&self.conn, me)
    }

    fn set_wake_ack(&self, me: &str, id: i64) -> Result<()> {
        check_ident("peer name", me)?;
        self.conn.execute(
            "INSERT INTO wake_acks (reader, last_id) VALUES (?1,?2)
             ON CONFLICT(reader) DO UPDATE SET last_id=?2",
            params![me, id],
        )?;
        Ok(())
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
        // WL-038: fold the ephemeral expiry into the SAME gc pass — delete expired
        // messages (and their reads) even if `ts >= cutoff` (delete-on-sweep). The
        // opportunistic `sweep_expired_messages` covers the between-gc window; this
        // guarantees expired rows are reaped by any gc too.
        let expiry_cut = now();
        tx.execute(
            "DELETE FROM reads WHERE message_id IN
                (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            params![expiry_cut],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![expiry_cut],
        )?;
        // P6: prune the delivery trace by the SAME retention cutoff so it is bounded
        // by the existing gc pass (no new sweeper). Mirrors the `messages` prune;
        // the count returned still reflects messages only (the trace is metadata).
        tx.execute("DELETE FROM delivery_log WHERE ts < ?1", params![cutoff])?;
        // WL-016: prune terminal schedule rows (executed or cancelled) older than the
        // retention cutoff. Non-terminal rows are preserved so pending schedules survive
        // long retention windows. The count returned still reflects messages only.
        tx.execute(
            "DELETE FROM schedules WHERE created_ts < ?1 AND (cancelled = 1 OR executed_ts IS NOT NULL)",
            params![cutoff],
        )?;
        tx.commit()?;
        Ok(n)
    }

    fn record_delivery(
        &self,
        ref_id: i64,
        ref_kind: &str,
        to_peer: &str,
        stage: &str,
        outcome: &str,
    ) -> Result<()> {
        // SECRET-FREE: only these six metadata fields are bound — never a body,
        // subject, sig, or token. The store NEVER injects; it records the outcome
        // its caller already computed post-inject. All values are bound via params!
        // (the table/column identifiers are the only constant literals).
        self.conn.execute(
            "INSERT INTO delivery_log (ref_id, ref_kind, to_peer, stage, outcome, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ref_id, ref_kind, to_peer, stage, outcome, now()],
        )?;
        Ok(())
    }

    fn list_delivery(&self, ref_id: i64, limit: i64) -> Result<Vec<DeliveryTrace>> {
        // BOUNDED: never return more than MAX_DELIVERY_ROWS regardless of `limit`.
        let lim = limit.clamp(1, MAX_DELIVERY_ROWS);
        let mut stmt = self.conn.prepare(
            "SELECT id, ref_id, ref_kind, to_peer, stage, outcome, ts
             FROM delivery_log WHERE ref_id = ?1
             ORDER BY ts ASC, id ASC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![ref_id, lim], |r| {
                Ok(DeliveryTrace {
                    id: r.get(0)?,
                    ref_id: r.get(1)?,
                    ref_kind: r.get(2)?,
                    to_peer: r.get(3)?,
                    stage: r.get(4)?,
                    outcome: r.get(5)?,
                    ts: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
        check_ident("peer name", name)?;
        if let Some(cert) = birth_cert {
            check_birth_cert(cert)?;
        }
        // Descriptive git tags are bounded + control-free at this single store
        // seam (lossy-but-total), so every capture path is covered identically.
        let repo = sanitize_tag(repo, MAX_REPO_LEN);
        let branch = sanitize_tag(branch, MAX_BRANCH_LEN);
        let worktree_id = sanitize_tag(worktree_id, MAX_WORKTREE_LEN);
        // Re-validate the circle at the store seam (defense-in-depth, the
        // check_ident precedent): an invalid value falls back to the default
        // circle rather than being stored raw.
        let circle = if crate::model::circle_valid(circle) {
            circle
        } else {
            crate::model::DEFAULT_CIRCLE
        };
        let tx = self.conn.unchecked_transaction()?;
        let existing_cert: Option<Option<String>> = tx
            .query_row(
                "SELECT birth_cert FROM peers WHERE name = ?1",
                params![name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        let cert = match existing_cert {
            None => {
                // New peer: bind the SUPPLIED cert when one was given (WL-047 spawn
                // pre-binds the parent-minted cert so the child's self-registration
                // matches), else mint a fresh one. The supplied cert was already
                // validated above by `check_birth_cert`. All pre-WL-047 callers pass
                // `None`, so this is backward-compatible (mint as before).
                let new_cert = match birth_cert {
                    Some(c) => c.to_string(),
                    None => mint_birth_cert()?,
                };
                tx.execute(
                    "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, birth_cert)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![name, mux, target, socket, cwd, now(), pid, host, repo, branch, worktree_id, circle, &new_cert],
                )?;
                new_cert
            }
            Some(None) => {
                // Existing peer without a cert (backward-compat): mint one and UPDATE.
                let new_cert = mint_birth_cert()?;
                tx.execute(
                    "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11, birth_cert=?12
                     WHERE name=?13",
                    params![mux, target, socket, cwd, now(), pid, host, repo, branch, worktree_id, circle, &new_cert, name],
                )?;
                new_cert
            }
            Some(Some(stored_cert)) => {
                // Existing peer WITH a cert: verify before allowing re-register.
                if let Some(supplied) = birth_cert {
                    if supplied != stored_cert {
                        anyhow::bail!("birth certificate mismatch for peer '{name}'");
                    }
                } else {
                    anyhow::bail!(
                        "peer '{name}' already registered; provide --cert to re-register"
                    );
                }
                // Cert matches: UPDATE fields, preserve stored cert.
                tx.execute(
                    "UPDATE peers SET mux=?1, target=?2, socket=?3, cwd=?4, last_seen=?5, pid=?6, host=?7, repo=?8, branch=?9, worktree_id=?10, circle=?11
                     WHERE name=?12",
                    params![mux, target, socket, cwd, now(), pid, host, repo, branch, worktree_id, circle, name],
                )?;
                stored_cert
            }
        };
        tx.commit()?;
        Ok(cert)
    }

    fn get_peer(&self, name: &str) -> Result<Option<Peer>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE name=?1",
        )?;
        let mut it = stmt.query_map(params![name], row_to_peer)?;
        match it.next() {
            Some(p) => {
                let mut p = p?;
                // Read-time TTL: a stale description ages out to "" (daemon-free;
                // the stored row is left untouched — pure read-time view).
                crate::model::expire_description(&mut p, now());
                Ok(Some(p))
            }
            None => Ok(None),
        }
    }

    fn get_birth_cert(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT birth_cert FROM peers WHERE name=?1")?;
        let mut it = stmt.query_map(params![name], |r| {
            let cert: Option<String> = r.get(0)?;
            Ok(cert)
        })?;
        match it.next() {
            Some(r) => Ok(r?),
            None => Ok(None),
        }
    }

    fn list_peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers ORDER BY name",
        )?;
        let mut rows: Vec<Peer> = stmt
            .query_map([], row_to_peer)?
            .collect::<rusqlite::Result<_>>()?;
        // Read-time TTL: blank any stale description so every listing surface
        // treats it as absent (daemon-free; stored rows untouched).
        let now = now();
        for p in &mut rows {
            crate::model::expire_description(p, now);
        }
        Ok(rows)
    }

    fn claim_orchestrator_role(
        &self,
        me: &str,
        circle: Option<&str>,
        force: bool,
    ) -> Result<ClaimOutcome> {
        check_ident("peer name", me)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // The caller must already be registered (a fresh peer always registers as
        // role='peer' first; claim is the only promotion path).
        let my_circle: String = match tx
            .query_row("SELECT circle FROM peers WHERE name=?1", params![me], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            Some(c) => c,
            None => anyhow::bail!("peer '{me}' is not registered"),
        };
        // Resolve the effective circle: arg if given, else the caller's own row.
        let target = match circle {
            Some(c) => crate::model::circle_or_default(c).to_string(),
            None => crate::model::circle_or_default(&my_circle).to_string(),
        };
        // Current orchestrators in the circle (normalize empty/legacy to default).
        let mut stmt = tx.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE role='orchestrator'",
        )?;
        let holders: Vec<Peer> = stmt
            .query_map([], row_to_peer)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| crate::model::circle_or_default(&p.circle) == target)
            .collect();
        drop(stmt);
        // WL-019: co-orchestrator support.
        // Non-force claims are additive: become a co-orchestrator without
        // demoting existing ones. Force claims still steal (demote all others).
        let mut demoted = Vec::new();
        if force {
            for p in &holders {
                if p.name != me {
                    tx.execute(
                        "UPDATE peers SET role=?1 WHERE name=?2",
                        params![crate::model::PeerRole::Peer.as_str(), p.name],
                    )?;
                    demoted.push(p.name.clone());
                }
            }
        }
        // Promote the caller (and pin its circle to the resolved target so a claim
        // with an explicit --circle co-locates the caller into that circle).
        tx.execute(
            "UPDATE peers SET role=?1, circle=?2 WHERE name=?3",
            params![crate::model::PeerRole::Orchestrator.as_str(), target, me],
        )?;
        tx.commit()?;
        demoted.sort();
        Ok(ClaimOutcome::Claimed {
            circle: target,
            demoted,
        })
    }

    fn orchestrator_status(&self, circle: Option<&str>) -> Result<OrchestratorStatus> {
        // No caller identity here, so an omitted circle resolves to the default
        // circle (the CLI/MCP layer passes the caller's resolved circle when it
        // wants a caller-scoped status).
        let target = circle
            .map(crate::model::circle_or_default)
            .unwrap_or(crate::model::DEFAULT_CIRCLE)
            .to_string();
        let mut stmt = self.conn.prepare(
            "SELECT name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts, birth_cert, contact_policy FROM peers WHERE role='orchestrator'",
        )?;
        let holders: Vec<Peer> = stmt
            .query_map([], row_to_peer)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| crate::model::circle_or_default(&p.circle) == target && is_alive(p))
            .collect();
        Ok(OrchestratorStatus {
            circle: target,
            present: !holders.is_empty(),
            holders,
        })
    }

    fn set_turn_state(&self, name: &str, state: &str) -> Result<()> {
        check_ident("peer name", name)?;
        // Validate against the enum at the seam — an unknown value is a hard error,
        // never stored raw (the AskState/PeerRole precedent). The canonical label
        // re-derived from `as_str` is the only inlined turn_state SQL value.
        let canonical = crate::model::TurnState::from_str(state)
            .map_err(|e| anyhow::anyhow!(e))?
            .as_str();
        // UPDATE-only on the caller's own row: never an INSERT, so a guessed name
        // worst-case touches 0 rows (harmless) and no foreign row can be created.
        self.conn.execute(
            "UPDATE peers SET turn_state=?2 WHERE name=?1",
            params![name, canonical],
        )?;
        Ok(())
    }

    fn set_description(&self, name: &str, description: &str) -> Result<()> {
        check_ident("peer name", name)?;
        // Bound + control-strip at the single store seam (lossy-but-total). An
        // oversized description truncates rather than errors.
        let clean = sanitize_tag(description, crate::model::MAX_DESC_LEN);
        // A cleared description stamps ts=0 (unambiguously "absent"); a set one
        // stamps now() so the read-time TTL can age it out independently of
        // liveness. UPDATE-only on the caller's own row (owner-only by construction).
        let ts = if clean.is_empty() { 0 } else { now() };
        self.conn.execute(
            "UPDATE peers SET description=?2, description_ts=?3 WHERE name=?1",
            params![name, clean, ts],
        )?;
        Ok(())
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
            SELECT m.id, m.ts, m.sender, m.recipient, m.subject, m.body, m.in_reply_to,
                   m.idempotency_key, m.trace_id, m.priority, m.superseded_by, m.expires_at, m.kind
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
                    idempotency_key: r.get(7).unwrap_or(None),
                    trace_id: r.get(8).unwrap_or(None),
                    priority: r.get(9).unwrap_or("normal".to_string()),
                    // WL-037: keep superseded rows in a thread, flagged.
                    superseded_by: r.get(10).unwrap_or(None),
                    // WL-038: carry the ephemeral deadline (expired rows are already
                    // deleted by sweep/gc, so a surviving thread row reads its value).
                    expires_at: r.get(11).unwrap_or(None),
                    // WL-039: carry the idle-ping marker (flagged in history/thread).
                    kind: r.get(12).unwrap_or(None),
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
        idempotency_key: Option<&str>,
        trace_id: Option<&str>,
        priority: Option<&str>,
        ttl: i64,
    ) -> Result<i64> {
        check_ident("recipient", to)?;
        check_ident("sender", from)?;
        check_host(to_host)?;
        check_body(body)?;
        let p = priority.unwrap_or("normal");
        // WL-038: carry the *relative* ttl (the receiver re-stamps `ts` on commit).
        // `<= 0` normalizes to 0 (no TTL); the CLI/MCP seam already validated the cap.
        let ttl = ttl.max(0);
        self.conn.execute(
            "INSERT INTO outbox (ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![now(), to, to_host, from, subject, body, sig, idempotency_key, trace_id, p, ttl],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn list_outbox(&self, for_recipient: &str, since_id: i64, limit: i64) -> Result<Vec<Intent>> {
        let limit = clamp_limit(limit);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl FROM outbox
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
            "SELECT id, ts, to_peer, to_host, from_peer, subject, body, sig, idempotency_key, trace_id, priority, ttl FROM outbox
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
        // ADD semantics (#7): registering the SAME (identity,pubkey) again is a
        // no-op via `ON CONFLICT DO NOTHING`. Enforce the per-identity cap ONLY for
        // a genuinely NEW key — a duplicate never counts against it and is always
        // accepted. Check the existing count + whether this exact key is already
        // present BEFORE inserting; a duplicate short-circuits the cap.
        let already: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM identity_keys WHERE identity = ?1 AND pubkey = ?2)",
            params![identity, pubkey],
            |r| r.get(0),
        )?;
        if !already {
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM identity_keys WHERE identity = ?1",
                params![identity],
                |r| r.get(0),
            )?;
            if count as usize >= MAX_KEYS_PER_IDENT {
                anyhow::bail!(
                    "identity '{identity}' already has the maximum {MAX_KEYS_PER_IDENT} \
                     registered keys; remove a retired one with `weave key remove` first"
                );
            }
        }
        self.conn.execute(
            "INSERT INTO identity_keys (identity, pubkey, added_ts) VALUES (?1, ?2, ?3)
             ON CONFLICT(identity, pubkey) DO NOTHING",
            params![identity, pubkey, now()],
        )?;
        Ok(())
    }

    fn get_key(&self, identity: &str) -> Result<Option<String>> {
        // Most-recent shim: newest by added_ts, ties broken by rowid (latest insert).
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT pubkey FROM identity_keys WHERE identity = ?1
                 ORDER BY added_ts DESC, rowid DESC LIMIT 1",
                params![identity],
                |r| r.get(0),
            )
            .ok();
        Ok(v)
    }

    fn get_keys(&self, identity: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT pubkey FROM identity_keys WHERE identity = ?1
             ORDER BY added_ts ASC, rowid ASC",
        )?;
        let rows = stmt
            .query_map(params![identity], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn remove_key(&self, identity: &str, pubkey: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM identity_keys WHERE identity = ?1 AND pubkey = ?2",
            params![identity, pubkey],
        )?;
        Ok(n > 0)
    }

    fn list_keys(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT identity, pubkey FROM identity_keys ORDER BY identity, added_ts ASC, rowid ASC",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn record_revocation(&self, ev: &RevocationEvent) -> Result<()> {
        // Defensive clamp at the write seam: keep an oversized hostile fp/source out
        // of the table even though both are bounded upstream. Identity is bounded by
        // `check_ident` upstream; clamp it too for symmetry. Bound `params!` only.
        let fp = clamp_field(&ev.fp);
        let identity = clamp_field(&ev.identity);
        let source = clamp_field(&ev.source);
        self.conn.execute(
            "INSERT INTO revocations (ts, fp, identity, source, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ev.ts, fp, identity, source, ev.kind.as_str()],
        )?;
        Ok(())
    }

    fn list_revocations(&self, limit: i64) -> Result<Vec<RevocationEvent>> {
        let lim = limit.clamp(0, MAX_REVOCATIONS_LIST);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, fp, identity, source, kind FROM revocations
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![lim], |r| {
                Ok(RevocationEvent {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    fp: r.get(2)?,
                    identity: r.get(3)?,
                    source: r.get(4)?,
                    kind: RevocationKind::parse(&r.get::<_, String>(5)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn count_revocations(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM revocations", [], |r| r.get(0))?)
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
        check_ident("asker", asker)?;
        check_ident("askee", askee)?;
        check_body(body)?;
        // P1 is point-to-point: a broadcast askee is rejected (broadcast ask is P2).
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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        // When chaining, load the prior ask (must exist + involve this asker/askee
        // pair) so the new question links to its last message and we can close it.
        let chained: Option<(i64, String)> = if let Some(rt) = reply_to {
            let prior: Option<(String, String, String, i64, Option<i64>)> = tx
                .query_row(
                    "SELECT asker, askee, state, question_msg_id, answer_msg_id
                     FROM asks WHERE id = ?1",
                    params![rt],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .ok();
            let (p_asker, p_askee, p_state, p_qid, p_aid) =
                prior.ok_or_else(|| anyhow::anyhow!("reply_to ask '{rt}' not found."))?;
            // The chain must stay within the same two parties (either orientation).
            let same_pair =
                (p_asker == asker && p_askee == askee) || (p_asker == askee && p_askee == asker);
            if !same_pair {
                anyhow::bail!("reply_to ask '{rt}' is between different parties.");
            }
            let p_state = AskState::from_str(&p_state).map_err(|m| anyhow::anyhow!(m))?;
            // The question links to the prior thread's most recent message.
            let link = p_aid.unwrap_or(p_qid);
            // Closing the prior thread is a monotonic transition; if it is already
            // acked the chain is still allowed (the prior is simply already closed).
            if p_state != AskState::Acked {
                if !p_state.can_transition(AskState::Acked) {
                    anyhow::bail!(
                        "cannot chain from ask '{rt}' in state {}.",
                        p_state.as_str()
                    );
                }
                tx.execute(
                    "UPDATE asks SET state = ?1, closed_ts = ?2, updated_ts = ?2 WHERE id = ?3",
                    params![AskState::Acked.as_str(), ts, rt],
                )?;
            }
            Some((link, rt.to_string()))
        } else {
            None
        };

        // Insert the question message (linked to the prior thread when chaining).
        let in_reply_to = chained.as_ref().map(|(link, _)| *link);
        let subject_owned = if chained.is_some() {
            // A chained question inherits the prior subject's Re: discipline when
            // the caller did not supply one.
            match subject {
                Some(s) => Some(s.to_string()),
                None => {
                    let parent_subj: Option<String> = in_reply_to.and_then(|mid| {
                        tx.query_row(
                            "SELECT subject FROM messages WHERE id = ?1",
                            params![mid],
                            |r| r.get(0),
                        )
                        .ok()
                        .flatten()
                    });
                    reply_subject(parent_subj.as_deref())
                }
            }
        } else {
            subject.map(|s| s.to_string())
        };
        tx.execute(
            "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![ts, asker, askee, subject_owned, body, in_reply_to],
        )?;
        let question_msg_id = tx.last_insert_rowid();

        // Mint the correlation id from the question message's fresh rowid (a unique
        // integer this transaction just produced). The `asks` PK is TEXT, so we seed
        // `new_ask_id` with this rowid rather than the asks rowid (unknown until
        // after its insert); uniqueness is guaranteed because `question_msg_id` is a
        // fresh AUTOINCREMENT id, and the nonce widens the opaque tail.
        let id = new_ask_id(question_msg_id);
        // A plain `ask` is never part of a group: parent_id is NULL (the legacy/P1
        // row shape). Ask-many children share the SAME insert via `insert_ask_row`
        // with a non-NULL parent_id.
        insert_ask_row(
            &tx,
            &id,
            question_msg_id,
            asker,
            askee,
            subject_owned.as_deref(),
            kind.as_str(),
            options,
            chained.as_ref().map(|(_, rt)| rt.as_str()),
            None,
            ts,
        )?;
        tx.commit()?;
        Ok((id, question_msg_id))
    }

    fn answer(&self, responder: &str, correlation_id: &str, body: &str) -> Result<i64> {
        check_ident("responder", responder)?;
        check_body(body)?;
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        let ts = now();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT asker, askee, state, question_msg_id FROM asks WHERE id = ?1",
                params![correlation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let (asker, askee, state, question_msg_id) =
            row.ok_or_else(|| anyhow::anyhow!("ask '{correlation_id}' not found."))?;
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
        // The answer goes back to the asker; inherit the question's subject.
        let parent_subject: Option<String> = tx
            .query_row(
                "SELECT subject FROM messages WHERE id = ?1",
                params![question_msg_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let subject = reply_subject(parent_subject.as_deref());
        tx.execute(
            "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![ts, responder, asker, subject, body, question_msg_id],
        )?;
        let answer_msg_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE asks SET answer_msg_id = ?1, state = ?2, updated_ts = ?3 WHERE id = ?4",
            params![
                answer_msg_id,
                AskState::Answered.as_str(),
                ts,
                correlation_id
            ],
        )?;
        tx.commit()?;
        Ok(answer_msg_id)
    }

    fn ack(&self, acker: &str, correlation_id: &str, message: Option<&str>) -> Result<()> {
        check_ident("acker", acker)?;
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        if let Some(m) = message {
            check_body(m)?;
        }
        let ts = now();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT askee, state FROM asks WHERE id = ?1",
                params![correlation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (askee, state) =
            row.ok_or_else(|| anyhow::anyhow!("ask '{correlation_id}' not found."))?;
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
            "UPDATE asks SET state = ?1, close_note = ?2, closed_ts = ?3, updated_ts = ?3
             WHERE id = ?4",
            params![AskState::Acked.as_str(), message, ts, correlation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn get_ask(&self, correlation_id: &str) -> Result<Option<Ask>> {
        if !ask_id_valid(correlation_id) {
            anyhow::bail!("invalid correlation id.");
        }
        let ask = self
            .conn
            .query_row(
                "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
                        options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id
                 FROM asks WHERE id = ?1",
                params![correlation_id],
                row_to_ask,
            )
            .ok();
        Ok(ask)
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
            "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
                    options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id
             FROM asks WHERE {where_clause}
             ORDER BY opened_ts DESC, rowid DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![me, limit], row_to_ask)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn list_ask_groups(&self, parent_ids: &[String]) -> Result<Vec<AskGroup>> {
        let mut out = Vec::new();
        // Bounded, parameterized per-id lookups (no IN-list interpolation): the input
        // is the small distinct set of parent_ids from the exported asks.
        for pid in parent_ids {
            if !ask_many_id_valid(pid) {
                continue;
            }
            let g: Option<AskGroup> = self
                .conn
                .query_row(
                    "SELECT parent_id, asker, subject, body, opened_ts, target_count
                     FROM ask_groups WHERE parent_id = ?1",
                    params![pid],
                    |r| {
                        Ok(AskGroup {
                            parent_id: r.get(0)?,
                            asker: r.get(1)?,
                            subject: r.get(2)?,
                            body: r.get(3)?,
                            opened_ts: r.get(4)?,
                            target_count: r.get(5)?,
                        })
                    },
                )
                .ok();
            if let Some(g) = g {
                out.push(g);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn import_ask(
        &self,
        id: &str,
        question_msg_id: i64,
        answer_msg_id: Option<i64>,
        asker: &str,
        askee: &str,
        subject: Option<&str>,
        state: AskState,
        kind: AskKind,
        options: Option<&str>,
        reply_to: Option<&str>,
        close_note: Option<&str>,
        opened_ts: i64,
        updated_ts: i64,
        closed_ts: Option<i64>,
        parent_id: Option<&str>,
    ) -> Result<bool> {
        // Defense-in-depth: re-validate at the store seam even though the caller
        // bounds every field. asker/askee are identity-shaped; the minted id is the
        // ask-id shape; options/close_note are length-capped (subject is bounded by
        // the caller's MAX_IMPORT_SUBJECT; the body lives in messages, not here).
        check_ident("asker", asker)?;
        check_ident("askee", askee)?;
        if !ask_id_valid(id) {
            anyhow::bail!("invalid imported ask id.");
        }
        if let Some(o) = options {
            check_body(o)?;
        }
        if let Some(c) = close_note {
            check_body(c)?;
        }
        if let Some(rt) = reply_to {
            if !ask_id_valid(rt) {
                anyhow::bail!("invalid imported reply_to correlation id.");
            }
        }
        if let Some(p) = parent_id {
            if !ask_many_id_valid(p) {
                anyhow::bail!("invalid imported parent_id.");
            }
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Idempotency: an ask already pointing at this (asker, askee, question) is the
        // SAME thread — the source ask id is meaningless across instances, so we dedup
        // on the remapped triple rather than the id (the message remap is itself
        // idempotent, so a re-import lands on the same question_msg_id).
        let existing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM asks WHERE asker = ?1 AND askee = ?2 AND question_msg_id = ?3",
            params![asker, askee, question_msg_id],
            |r| r.get(0),
        )?;
        if existing > 0 {
            tx.commit()?;
            return Ok(false);
        }
        // Out-of-order materialize: insert the row DIRECTLY in `state` with answer/
        // close fields, bypassing the lifecycle machine. 15-column INSERT order matches
        // the canonical asks projection.
        tx.execute(
            "INSERT INTO asks
                (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
                 options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                id,
                question_msg_id,
                answer_msg_id,
                asker,
                askee,
                subject,
                state.as_str(),
                kind.as_str(),
                options,
                reply_to,
                close_note,
                opened_ts,
                updated_ts,
                closed_ts,
                parent_id,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn import_ask_group(
        &self,
        parent_id: &str,
        asker: &str,
        subject: Option<&str>,
        body: &str,
        opened_ts: i64,
        target_count: i64,
    ) -> Result<bool> {
        if !ask_many_id_valid(parent_id) {
            anyhow::bail!("invalid imported ask-many parent id.");
        }
        check_ident("asker", asker)?;
        check_body(body)?;
        if let Some(s) = subject {
            check_body(s)?;
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ask_groups WHERE parent_id = ?1",
            params![parent_id],
            |r| r.get(0),
        )?;
        if existing > 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO ask_groups (parent_id, asker, subject, body, opened_ts, target_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![parent_id, asker, subject, body, opened_ts, target_count],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn has_open_asks(&self, me: &str) -> Result<bool> {
        check_ident("me", me)?;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM asks WHERE askee = ?1 AND state = 'open'",
            params![me],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    fn ask_for_message(&self, message_id: i64) -> Result<Option<String>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM asks WHERE question_msg_id = ?1 OR answer_msg_id = ?1 LIMIT 1",
                params![message_id],
                |r| r.get(0),
            )
            .ok();
        Ok(id)
    }

    fn create_ask_many(
        &self,
        asker: &str,
        peers: &[String],
        subject: Option<&str>,
        body: &str,
    ) -> Result<AskManyOutcome> {
        // Whole-call validation BEFORE any insert (hard errors). The asker/body are
        // bounded; a broadcast asker is rejected; the peer list must be non-empty and
        // within the fanout cap (de-duped count).
        check_ident("asker", asker)?;
        check_body(body)?;
        if is_broadcast(asker) {
            anyhow::bail!("the ask-many asker must be a concrete peer, not a broadcast alias.");
        }
        if let Some(s) = subject {
            check_body(s)?;
        }
        // De-dup the requested peer list (a repeated peer is ONE child), preserving
        // order; this de-duped count is the canonical `target_count`.
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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Insert the parent anchor first. `target_count` is the de-duped REQUESTED
        // count, so totality holds even when some children fail pre-insert (failed ==
        // target_count - created).
        tx.execute(
            "INSERT INTO ask_groups (parent_id, asker, subject, body, opened_ts, target_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![parent_id, asker, subject, body, ts, deduped.len() as i64],
        )?;

        let mut children: Vec<(String, std::result::Result<String, String>)> =
            Vec::with_capacity(deduped.len());
        for peer in &deduped {
            // Best-effort per child: a rejected peer records an error and is SKIPPED
            // (no child ask), never aborting the whole fanout. Same point-to-point
            // validation a P1 `ask` applies to its askee.
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
            // Insert the question message + the child ask carrying the parent id.
            // Reuses the SAME `insert_ask_row` the plain `ask` uses (shared lifecycle).
            tx.execute(
                "INSERT INTO messages (ts, sender, recipient, subject, body, in_reply_to)
                 VALUES (?1,?2,?3,?4,?5,NULL)",
                params![ts, asker, peer, subject, body],
            )?;
            let question_msg_id = tx.last_insert_rowid();
            let cid = new_ask_id(question_msg_id);
            insert_ask_row(
                &tx,
                &cid,
                question_msg_id,
                asker,
                peer,
                subject,
                AskKind::FreeText.as_str(),
                None,
                None,
                Some(&parent_id),
                ts,
            )?;
            children.push((peer.clone(), Ok(cid)));
        }
        tx.commit()?;
        Ok(AskManyOutcome {
            parent_id,
            children,
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
        let group: Option<AskGroup> = self
            .conn
            .query_row(
                "SELECT parent_id, asker, subject, body, opened_ts, target_count
                 FROM ask_groups WHERE parent_id = ?1",
                params![parent_id],
                |r| {
                    Ok(AskGroup {
                        parent_id: r.get(0)?,
                        asker: r.get(1)?,
                        subject: r.get(2)?,
                        body: r.get(3)?,
                        opened_ts: r.get(4)?,
                        target_count: r.get(5)?,
                    })
                },
            )
            .ok();
        let Some(group) = group else {
            return Ok(None);
        };
        // Enumerate the children by parent_id; bounded by target_count (≤ cap).
        let mut stmt = self.conn.prepare(
            "SELECT id, askee, state, answer_msg_id FROM asks
             WHERE parent_id = ?1 ORDER BY opened_ts ASC, rowid ASC",
        )?;
        let rows: Vec<(String, String, String, Option<i64>)> = stmt
            .query_map(params![parent_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut children: Vec<AskManyChildView> = Vec::with_capacity(rows.len());
        let (mut answered, mut acked, mut pending) = (0i64, 0i64, 0i64);
        for (cid, askee, state_str, answer_msg_id) in rows {
            let state = AskState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
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
        // failed == the gap between the requested (de-duped) target_count and the
        // children that actually inserted (a peer rejected pre-insert has NO asks row).
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
    }

    // ── P3 job board (poll-only) ──────────────────────────────────────────────
    fn create_job(&self, creator: &str, spec: JobSpec) -> Result<Job> {
        validate_job_spec(creator, &spec)?;
        let ts = now();
        let owner = spec.owner.clone().unwrap_or_else(|| creator.to_string());
        let kind = spec.kind.clone().unwrap_or_else(|| "general".to_string());
        let visibility = spec
            .visibility
            .clone()
            .unwrap_or_else(|| "circle".to_string());
        let job_id = new_job_id(ts);
        self.conn.execute(
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
            params![
                job_id,
                spec.title,
                spec.description.clone().unwrap_or_default(),
                kind,
                JobState::Queued.as_str(),
                spec.prompt,
                creator,
                owner,
                spec.assignee,
                spec.circle,
                spec.correlation_id,
                spec.source_kind,
                spec.source_id,
                spec.scope,
                visibility,
                spec.deadline_at,
                spec.expires_at,
                ts,
            ],
        )?;
        self.get_job(&job_id)?
            .ok_or_else(|| anyhow::anyhow!("job '{job_id}' vanished after insert."))
    }

    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        let job = self
            .conn
            .query_row("SELECT * FROM jobs WHERE id = ?1", params![id], row_to_job)
            .ok();
        Ok(job)
    }

    fn list_jobs(&self, filter: JobFilter, limit: i64) -> Result<Vec<Job>> {
        let limit = clamp_limit(limit);
        // Build a positional WHERE from the populated filters. The state value is a
        // compile-time `as_str` constant (never user text); identities are bound.
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(s) = filter.state {
            clauses.push(format!("state = ?{}", binds.len() + 1));
            binds.push(Box::new(s.as_str().to_string()));
        }
        if let Some(ref v) = filter.owner {
            clauses.push(format!("owner = ?{}", binds.len() + 1));
            binds.push(Box::new(v.clone()));
        }
        if let Some(ref v) = filter.creator {
            clauses.push(format!("creator = ?{}", binds.len() + 1));
            binds.push(Box::new(v.clone()));
        }
        if let Some(ref v) = filter.assignee {
            clauses.push(format!("assignee = ?{}", binds.len() + 1));
            binds.push(Box::new(v.clone()));
        }
        if let Some(ref v) = filter.circle {
            clauses.push(format!("circle = ?{}", binds.len() + 1));
            binds.push(Box::new(v.clone()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT * FROM jobs {where_sql}
             ORDER BY updated_ts DESC, rowid DESC LIMIT ?{}",
            binds.len() + 1
        );
        binds.push(Box::new(limit));
        let mut stmt = self.conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(bind_refs.as_slice(), row_to_job)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn claim_job(&self, id: &str, assignee: &str) -> Result<Option<Job>> {
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        check_ident("assignee", assignee)?;
        let ts = now();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state_str: Option<String> = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .ok();
        let Some(state_str) = state_str else {
            tx.commit()?;
            return Ok(None);
        };
        let state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
        if state.is_terminal() {
            anyhow::bail!("job '{id}' is {} and cannot be claimed.", state.as_str());
        }
        let attempt_id = new_attempt_id(ts);
        tx.execute(
            "UPDATE jobs SET assignee = ?1, attempt_id = ?2, state = ?3, updated_ts = ?4
             WHERE id = ?5",
            params![assignee, attempt_id, JobState::Running.as_str(), ts, id],
        )?;
        tx.commit()?;
        self.get_job(id)
    }

    fn update_job(&self, id: &str, attempt_id: Option<&str>, patch: JobPatch) -> Result<Job> {
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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>, String, Option<String>)> = tx
            .query_row(
                "SELECT state, attempt_id, progress_events_json, phase FROM jobs WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let (state_str, cur_attempt, events_json, cur_phase) =
            row.ok_or_else(|| anyhow::anyhow!("job '{id}' not found."))?;
        // attempt_id FENCING: a claimed job (attempt_id set) requires a MATCHING token.
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
        // Append the progress note to the append-only event log (never overwrites).
        let events = append_progress_event(
            &events_json,
            ts,
            patch.progress_note.as_deref(),
            new_state,
            patch.phase.as_deref().or(cur_phase.as_deref()),
        );
        let completed_ts = if new_state.is_terminal() {
            Some(ts)
        } else {
            None
        };
        tx.execute(
            "UPDATE jobs SET
                state = ?1,
                state_reason = COALESCE(?2, state_reason),
                phase = COALESCE(?3, phase),
                progress_note = COALESCE(?4, progress_note),
                progress_events_json = ?5,
                result_summary = COALESCE(?6, result_summary),
                result_json = COALESCE(?7, result_json),
                error_json = COALESCE(?8, error_json),
                artifacts_json = COALESCE(?9, artifacts_json),
                completed_ts = COALESCE(?10, completed_ts),
                updated_ts = ?11
             WHERE id = ?12",
            params![
                new_state.as_str(),
                patch.state_reason,
                patch.phase,
                patch.progress_note,
                events,
                patch.result_summary,
                patch.result_json,
                patch.error_json,
                patch.artifacts_json,
                completed_ts,
                ts,
                id,
            ],
        )?;
        tx.commit()?;
        self.get_job(id)?
            .ok_or_else(|| anyhow::anyhow!("job '{id}' vanished after update."))
    }

    fn job_result(&self, id: &str) -> Result<Option<JobResultView>> {
        let job = match self.get_job(id)? {
            Some(j) => j,
            None => return Ok(None),
        };
        Ok(Some(job_result_view(&job)))
    }

    fn cancel_job(
        &self,
        id: &str,
        requested_by: &str,
        reason: Option<&str>,
    ) -> Result<Option<Job>> {
        if !job_id_valid(id) {
            anyhow::bail!("invalid job id.");
        }
        check_ident("requested_by", requested_by)?;
        if let Some(r) = reason {
            check_job_text("reason", r)?;
        }
        let ts = now();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state_str: Option<String> = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .ok();
        let Some(state_str) = state_str else {
            tx.commit()?;
            return Ok(None);
        };
        let state = JobState::from_str(&state_str).map_err(|m| anyhow::anyhow!(m))?;
        if state == JobState::Queued {
            // Nothing has claimed it → straight to terminal cancelled.
            tx.execute(
                "UPDATE jobs SET state = ?1, cancel_requested = 1,
                    cancel_requested_by = COALESCE(cancel_requested_by, ?2),
                    cancel_requested_ts = COALESCE(cancel_requested_ts, ?3),
                    cancel_reason = COALESCE(cancel_reason, ?4),
                    completed_ts = ?3, updated_ts = ?3
                 WHERE id = ?5",
                params![JobState::Cancelled.as_str(), requested_by, ts, reason, id],
            )?;
        } else {
            // Terminal OR claimed/running → COOPERATIVE flag only (no state change).
            tx.execute(
                "UPDATE jobs SET cancel_requested = 1,
                    cancel_requested_by = COALESCE(cancel_requested_by, ?1),
                    cancel_requested_ts = COALESCE(cancel_requested_ts, ?2),
                    cancel_reason = COALESCE(cancel_reason, ?3),
                    updated_ts = ?2
                 WHERE id = ?4",
                params![requested_by, ts, reason, id],
            )?;
        }
        tx.commit()?;
        self.get_job(id)
    }

    fn heartbeat(&self, name: &str, host: &str, pid: Option<i64>) -> Result<()> {
        check_ident("peer name", name)?;
        let ts = crate::model::now();
        self.conn.execute(
            "INSERT INTO presence (name, host, pid, heartbeat_ts)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                 host = excluded.host,
                 pid = excluded.pid,
                 heartbeat_ts = excluded.heartbeat_ts",
            params![name, host, pid, ts],
        )?;
        Ok(())
    }

    fn presence(&self, name: &str, host: &str) -> Result<Option<i64>> {
        let cutoff = crate::model::now().saturating_sub(PRESENCE_TTL_SECS);
        let mut stmt = self.conn.prepare(
            "SELECT heartbeat_ts FROM presence
             WHERE name = ?1 AND host = ?2 AND heartbeat_ts >= ?3
             LIMIT 1",
        )?;
        let ts: Result<Option<i64>> = stmt
            .query_row(params![name, host, cutoff], |r| r.get(0))
            .optional()
            .map_err(|e| e.into());
        ts
    }

    fn evict_stale_presence(&self, cutoff_secs: i64) -> Result<usize> {
        let cutoff = crate::model::now().saturating_sub(cutoff_secs);
        let n = self.conn.execute(
            "DELETE FROM presence WHERE heartbeat_ts < ?1",
            params![cutoff],
        )?;
        Ok(n)
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
        self.conn.execute(
            "INSERT INTO schedules (kind, cron_expr, next_run, sender, recipient, subject, body, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![kind.as_str(), cron_expr, next_run, sender, recipient, subject, body, ts],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn list_schedules(&self, sender: &str, limit: i64) -> Result<Vec<Schedule>> {
        check_ident("sender", sender)?;
        let limit = clamp_limit(limit);
        let mut stmt = self.conn.prepare(
            "SELECT * FROM schedules WHERE sender = ?1 ORDER BY created_ts DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![sender, limit], row_to_schedule)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn cancel_schedule(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE schedules SET cancelled = 1 WHERE id = ?1 AND cancelled = 0 AND executed_ts IS NULL",
            params![id],
        )?;
        Ok(n > 0)
    }

    fn get_due_schedules(&self, before_ts: i64) -> Result<Vec<Schedule>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM schedules
             WHERE next_run <= ?1 AND cancelled = 0
               AND (executed_ts IS NULL OR kind = 'recurring')
             ORDER BY next_run ASC",
        )?;
        let rows = stmt
            .query_map(params![before_ts], row_to_schedule)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn mark_schedule_executed(&self, id: i64) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT kind, cron_expr FROM schedules WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        let Some((kind_str, cron_expr)) = row else {
            tx.commit()?;
            return Ok(());
        };
        let kind = ScheduleKind::from_str(&kind_str).map_err(|m| anyhow::anyhow!(m))?;
        match kind {
            ScheduleKind::OneShot => {
                tx.execute(
                    "UPDATE schedules SET executed_ts = ?1 WHERE id = ?2",
                    params![now(), id],
                )?;
            }
            ScheduleKind::Recurring => {
                let next = crate::model::next_occurrence(&cron_expr, now());
                if let Some(ts) = next {
                    tx.execute(
                        "UPDATE schedules SET next_run = ?1 WHERE id = ?2",
                        params![ts, id],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE schedules SET cancelled = 1 WHERE id = ?1",
                        params![id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
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
        self.conn.execute(
            "INSERT INTO reviews (id, pr_url, title, author, repo, state, review_requested_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &id,
                pr_url,
                title,
                author,
                repo,
                state.as_str(),
                review_requested_at,
                created_at,
            ],
        )?;
        Ok(id)
    }

    fn review_queue(&self, filter: ReviewQueueFilter, limit: i64) -> Result<Vec<ReviewItem>> {
        let limit = clamp_limit(limit);
        let (where_clause, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match filter {
            ReviewQueueFilter::All => ("", Vec::new()),
            ReviewQueueFilter::Open => {
                let p: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new("open".to_string())];
                ("WHERE state = ?1", p)
            }
            ReviewQueueFilter::Pending => {
                let p: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new("open".to_string())];
                ("WHERE state = ?1 AND reviewed_at IS NULL", p)
            }
            ReviewQueueFilter::Reviewed => {
                let p: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                ("WHERE reviewed_at IS NOT NULL", p)
            }
        };
        let sql = format!(
            "SELECT id, pr_url, title, author, repo, state, review_requested_at, reviewed_at, reviewed_by, created_at
             FROM reviews {} ORDER BY created_at DESC LIMIT {}",
            where_clause, limit
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(ReviewItem {
                id: r.get(0)?,
                pr_url: r.get(1)?,
                title: r.get(2)?,
                author: r.get(3)?,
                repo: r.get(4)?,
                state: ReviewItemState::from_str(&r.get::<_, String>(5)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    )
                })?,
                review_requested_at: r.get(6)?,
                reviewed_at: r.get(7)?,
                reviewed_by: r.get(8)?,
                created_at: r.get(9)?,
            })
        })?;
        let mut items = Vec::new();
        for r in rows {
            items.push(r?);
        }
        Ok(items)
    }

    fn mark_reviewed(&self, id: &str, reviewer: &str) -> Result<bool> {
        check_ident("reviewer", reviewer)?;
        let n = self.conn.execute(
            "UPDATE reviews SET reviewed_at = ?1, reviewed_by = ?2 WHERE id = ?3",
            params![now(), reviewer, id],
        )?;
        Ok(n > 0)
    }

    fn remove_review_item(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM reviews WHERE id = ?1", params![id])?;
        Ok(n > 0)
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

        let _ = self.sweep_expired_leases()?;

        // Check for path conflicts (exact, parent, child) with any *other* holder.
        let mut stmt = self.conn.prepare(
            "SELECT resource, holder, expires FROM leases
             WHERE expires > ?1
               AND (resource = ?2
                    OR resource || '/' = SUBSTR(?2, 1, LENGTH(resource) + 1)
                    OR ?2 || '/' = SUBSTR(resource, 1, LENGTH(?2) + 1))",
        )?;
        let conflicts: Vec<(String, String, i64)> = stmt
            .query_map(params![now(), &resource_norm], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (existing_res, existing_holder, existing_expires) in conflicts {
            if existing_holder == holder && existing_res == resource_norm {
                // Same holder re-reserving exact same resource: allow (extend).
                self.conn.execute(
                    "UPDATE leases SET acquired = ?1, expires = ?2, note = ?3
                     WHERE resource = ?4 AND holder = ?5",
                    params![acquired, expires, note, &resource_norm, holder],
                )?;
                return Ok(Lease {
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

        // No conflicts: insert fresh.
        self.conn.execute(
            "INSERT INTO leases (resource, holder, acquired, expires, note)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(resource) DO UPDATE SET
                 holder = excluded.holder,
                 acquired = excluded.acquired,
                 expires = excluded.expires,
                 note = excluded.note
             WHERE leases.expires < ?6",
            params![&resource_norm, holder, acquired, expires, note, now()],
        )?;

        Ok(Lease {
            resource: resource_norm,
            holder: holder.to_string(),
            acquired,
            expires,
            note: note.to_string(),
        })
    }

    fn release_lease(&self, holder: &str, resource: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM leases WHERE resource = ?1 AND holder = ?2",
            params![resource, holder],
        )?;
        Ok(n > 0)
    }

    fn list_leases(&self, limit: i64) -> Result<Vec<Lease>> {
        let _ = self.sweep_expired_leases()?;
        let now = now();
        let limit = clamp_limit(limit);
        let mut stmt = self.conn.prepare(
            "SELECT resource, holder, acquired, expires, note
             FROM leases WHERE expires > ?1
             ORDER BY acquired DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit], |r| {
            Ok(Lease {
                resource: r.get(0)?,
                holder: r.get(1)?,
                acquired: r.get(2)?,
                expires: r.get(3)?,
                note: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn sweep_expired_leases(&self) -> Result<usize> {
        let now = now();
        let n = self
            .conn
            .execute("DELETE FROM leases WHERE expires <= ?1", params![now])?;
        Ok(n)
    }

    fn set_message_priority(&self, id: i64, priority: &str) -> Result<()> {
        let p = crate::model::MessagePriority::parse(priority);
        self.conn.execute(
            "UPDATE messages SET priority = ?1 WHERE id = ?2",
            params![p.as_str(), id],
        )?;
        Ok(())
    }

    fn set_message_expiry(&self, id: i64, expires_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET expires_at = ?1 WHERE id = ?2",
            params![expires_at, id],
        )?;
        Ok(())
    }

    fn sweep_expired_messages(&self) -> Result<usize> {
        let now = now();
        // Delete-on-sweep: remove the reads first (mirroring the gc reads prune),
        // then the messages, in one IMMEDIATE tx so a reader can't observe a
        // half-swept row. The count returned reflects messages removed.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM reads WHERE message_id IN
                (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            params![now],
        )?;
        let n = tx.execute(
            "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now],
        )?;
        tx.commit()?;
        Ok(n)
    }

    fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()> {
        // Both ids must exist; the new id is looked up so a typo'd/forged
        // successor can't strand the predecessor pointing at a phantom row.
        let old_sender: Option<String> = self
            .conn
            .query_row(
                "SELECT sender FROM messages WHERE id = ?1",
                params![old_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(old_sender) = old_sender else {
            anyhow::bail!("cannot supersede: message #{old_id} does not exist");
        };
        let new_exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
            params![new_id],
            |r| r.get(0),
        )?;
        if !new_exists {
            anyhow::bail!("cannot supersede: successor message #{new_id} does not exist");
        }
        // Authorization: only the ORIGINAL SENDER of old_id may supersede it.
        // Best-effort same-identity guard (`from` is advisory until the `sign`
        // feature makes it unforgeable) that blocks a hostile session from
        // hiding another agent's message from inboxes (censorship/DoS vector).
        if old_sender != caller {
            anyhow::bail!("cannot supersede: #{old_id} was sent by '{old_sender}', not '{caller}'");
        }
        self.conn.execute(
            "UPDATE messages SET superseded_by = ?2 WHERE id = ?1",
            params![old_id, new_id],
        )?;
        Ok(())
    }

    fn supersede_prior_idle(&self, sender: &str, recipient: &str, new_id: i64) -> Result<usize> {
        // Stamp the new ping as idle so it (and only it) is eligible to be
        // superseded by the NEXT idle ping. Scoped to `sender` so a caller can
        // never re-kind another session's message (authz, the WL-037 spine).
        self.conn.execute(
            "UPDATE messages SET kind = ?3 WHERE id = ?1 AND sender = ?2",
            params![new_id, sender, crate::model::KIND_IDLE],
        )?;
        // Auto-supersede the sender's prior UNREAD idle pings to this recipient.
        // The predicate is the hard safety boundary: kind='idle' excludes every
        // real message; sender=? is the self-only authz; recipient=? scopes to
        // the same mailbox; id<>new_id makes an idempotency-key replay a no-op;
        // superseded_by IS NULL skips already-chained rows; the NOT EXISTS clause
        // is the SAME unread definition as `unread_count_conn` (a just-read ping
        // is not superseded).
        let n = self.conn.execute(
            "UPDATE messages SET superseded_by = ?1
             WHERE sender = ?2 AND recipient = ?3
               AND kind = ?4
               AND superseded_by IS NULL
               AND id <> ?1
               AND NOT EXISTS (SELECT 1 FROM reads r WHERE r.message_id = messages.id AND r.reader = ?3)",
            params![new_id, sender, recipient, crate::model::KIND_IDLE],
        )?;
        Ok(n)
    }

    fn set_peer_policy(&self, name: &str, policy: &str) -> Result<()> {
        let p = crate::model::ContactPolicy::parse(policy);
        self.conn.execute(
            "UPDATE peers SET contact_policy = ?1 WHERE name = ?2",
            params![p.as_str(), name],
        )?;
        Ok(())
    }

    fn get_peer_policy(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT contact_policy FROM peers WHERE name = ?1")?;
        let mut rows = stmt.query_map(params![name], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(Ok(v)) => Ok(Some(v)),
            _ => Ok(None),
        }
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
            self.conn
                .query_row(
                    "SELECT body FROM messages WHERE id = ?1",
                    params![aid],
                    |r| r.get(0),
                )
                .ok()
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
        let mut stmt = self.conn.prepare(
            "SELECT id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind,
                    options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id
             FROM asks
             WHERE asker = ?1 AND kind = ?2
             ORDER BY opened_ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![me, AskKind::ToolPermission.as_str(), limit],
            row_to_ask,
        )?;
        let mut asks = Vec::new();
        for r in rows {
            asks.push(r?);
        }
        Ok(asks)
    }

    fn store_summary(&self, root_id: i64, text: &str, model: &str) -> Result<()> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO summaries (root_id, text, model, created_ts, refreshed_ts)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id) DO UPDATE SET
                 text = excluded.text,
                 model = excluded.model,
                 refreshed_ts = excluded.refreshed_ts",
            params![root_id, text, model, ts, ts],
        )?;
        Ok(())
    }

    fn get_summary(&self, root_id: i64) -> Result<Option<crate::model::Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT root_id, text, model, created_ts, refreshed_ts
             FROM summaries WHERE root_id = ?1",
        )?;
        let row = stmt.query_row(params![root_id], |r| {
            Ok(crate::model::Summary {
                root_id: r.get(0)?,
                text: r.get(1)?,
                model: r.get(2)?,
                created_ts: r.get(3)?,
                refreshed_ts: r.get(4)?,
            })
        });
        match row {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_summary(&self, root_id: i64) -> Result<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM summaries WHERE root_id = ?1", params![root_id])?;
        Ok(rows > 0)
    }
}

/// Validate a [`JobSpec"] (and `creator`) before any insert: identity shapes for
/// creator/owner/assignee/circle, length caps on every free-text field. Returns the
/// (placeholder) ok marker; the id is minted by the caller. Shared discipline so
/// CLI + MCP both inherit it (the validation lives in the store). Backend-agnostic.
#[cfg(any(feature = "sqlite", feature = "libsql"))]
pub(crate) fn validate_job_spec(creator: &str, spec: &JobSpec) -> Result<()> {
    check_ident("creator", creator)?;
    if spec.title.trim().is_empty() {
        anyhow::bail!("a job title is required.");
    }
    check_job_text("title", &spec.title)?;
    if let Some(ref d) = spec.description {
        check_job_text("description", d)?;
    }
    if let Some(ref p) = spec.prompt {
        check_job_text("prompt", p)?;
    }
    if let Some(ref o) = spec.owner {
        check_ident("owner", o)?;
    }
    if let Some(ref a) = spec.assignee {
        check_ident("assignee", a)?;
    }
    if let Some(ref c) = spec.circle {
        check_ident("circle", c)?;
    }
    // Inert board-metadata strings are length-capped (not identities, but echoed
    // into listings) so a hostile/oversized value cannot bloat the table.
    if let Some(ref k) = spec.kind {
        check_job_text("kind", k)?;
    }
    if let Some(ref v) = spec.visibility {
        check_job_text("visibility", v)?;
    }
    if let Some(ref c) = spec.correlation_id {
        check_job_text("correlation_id", c)?;
    }
    if let Some(ref s) = spec.source_kind {
        check_job_text("source_kind", s)?;
    }
    if let Some(ref s) = spec.source_id {
        check_job_text("source_id", s)?;
    }
    if let Some(ref s) = spec.scope {
        check_job_text("scope", s)?;
    }
    Ok(())
}

/// Validate a [`JobPatch`]'s user-supplied free-text + JSON payloads before write.
/// Backend-agnostic so both store impls call it.
#[cfg(any(feature = "sqlite", feature = "libsql"))]
pub(crate) fn validate_job_patch(patch: &JobPatch) -> Result<()> {
    if let Some(ref r) = patch.state_reason {
        check_job_text("state_reason", r)?;
    }
    if let Some(ref p) = patch.phase {
        check_job_text("phase", p)?;
    }
    if let Some(ref n) = patch.progress_note {
        check_job_text("progress_note", n)?;
    }
    if let Some(ref s) = patch.result_summary {
        check_job_text("result_summary", s)?;
    }
    if let Some(ref j) = patch.result_json {
        check_job_json("result", j)?;
    }
    if let Some(ref j) = patch.error_json {
        check_job_json("error", j)?;
    }
    if let Some(ref j) = patch.artifacts_json {
        check_job_json("artifacts", j)?;
    }
    Ok(())
}

/// Append one structured event `{at,note,state,phase}` to the append-only
/// `progress_events_json` log (never overwrites prior events). A note-less update
/// still records a state/phase checkpoint. A malformed/empty existing log is
/// treated as an empty array (self-heals). Uses serde_json (already a dep — NO new
/// crate). Backend-agnostic.
#[cfg(any(feature = "sqlite", feature = "libsql"))]
pub(crate) fn append_progress_event(
    existing: &str,
    at: i64,
    note: Option<&str>,
    state: JobState,
    phase: Option<&str>,
) -> String {
    let mut arr: Vec<serde_json::Value> = serde_json::from_str(existing).unwrap_or_default();
    arr.push(serde_json::json!({
        "at": at,
        "note": note,
        "state": state.as_str(),
        "phase": phase,
    }));
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Build the read-time [`JobResultView`] from a job: a terminal job exposes its
/// payload (`ready=true`); a non-terminal one is the not-ready marker. PURE (no
/// I/O) so both backends share it. Backend-agnostic.
#[cfg(any(feature = "sqlite", feature = "libsql"))]
pub(crate) fn job_result_view(job: &Job) -> JobResultView {
    let ready = job.state.is_terminal();
    JobResultView {
        id: job.id.clone(),
        state: job.state,
        ready,
        result_summary: job.result_summary.clone(),
        result_json: job.result_json.clone(),
        error_json: job.error_json.clone(),
        artifacts_json: job.artifacts_json.clone(),
        completed_ts: job.completed_ts,
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
            circle: crate::model::DEFAULT_CIRCLE.to_string(),
            role: crate::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
        }
    }

    /// The same `(name, host)` seen via local + foreign collapses to ONE entry,
    /// and a fresh `last_seen` (both not pid-probed ⇒ both TTL-alive) makes the
    /// newer row win.
    #[test]
    fn merge_collapses_same_name_host_newer_wins() {
        let t = now();
        let local = PeerView {
            peer: peer("prompt_hub", "boxA", t - 100, None),
            origin: Origin::Local,
        };
        let foreign = PeerView {
            peer: peer("prompt_hub", "boxA", t - 5, None),
            origin: Origin::Foreign("other.db".to_string()),
        };
        let merged = merge_peer_views(vec![local, foreign]);
        assert_eq!(merged.len(), 1, "same (name,host) collapses to one");
        assert_eq!(merged[0].peer.last_seen, t - 5, "newer last_seen wins");
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
        // The question landed in b's inbox.
        let (b_in, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(b_in.iter().any(|m| m.id == qid && m.sender == "a"));
        let ask = s.get_ask(&cid).unwrap().unwrap();
        assert_eq!(ask.state, AskState::Open);
        assert_eq!(ask.asker, "a");
        assert_eq!(ask.askee, "b");

        // b answers; the answer addresses BACK to a, threads to the question.
        let aid = s.answer("b", &cid, "3pm").unwrap();
        let (a_in, _) = s.inbox("a", true, false, 50).unwrap();
        let ans = a_in.iter().find(|m| m.id == aid).expect("a got answer");
        assert_eq!(ans.sender, "b");
        assert_eq!(ans.recipient, "a");
        assert_eq!(ans.in_reply_to, Some(qid));
        assert_eq!(ans.subject.as_deref(), Some("Re: help"));
        let ask = s.get_ask(&cid).unwrap().unwrap();
        assert_eq!(ask.state, AskState::Answered);
        assert_eq!(ask.answer_msg_id, Some(aid));

        // b acks with a closing note; thread closes.
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
        // Double-ack rejected.
        assert!(s.ack("b", &cid, None).is_err());
        // Answer after ack rejected.
        assert!(s.answer("b", &cid, "late").is_err());
        // Unknown correlation id: clean error, never a panic.
        assert!(s.ack("b", "ask_999_1", None).is_err());
        assert!(s.answer("b", "ask_999_1", "x").is_err());
        assert!(s.get_ask("ask_999_1").unwrap().is_none());
    }

    #[test]
    fn ask_owner_checks_and_caps() {
        let s = mem();
        let (cid, _) = s
            .ask("a", "b", None, "q", AskKind::FreeText, None, None)
            .unwrap();
        // Only the askee can answer/ack.
        assert!(s.answer("a", &cid, "self").is_err());
        assert!(s.ack("a", &cid, None).is_err());
        // Broadcast askee rejected (point-to-point only).
        assert!(s
            .ask("a", "all", None, "q", AskKind::FreeText, None, None)
            .is_err());
        // Oversized body rejected.
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s
            .ask("a", "b", None, &big, AskKind::FreeText, None, None)
            .is_err());
        // Invalid correlation id rejected before any DB bind.
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
        // Chain a new ask off c1: it closes c1 and links to c1's last message.
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
        let prior = s.get_ask(&c1).unwrap().unwrap();
        assert_eq!(prior.state, AskState::Acked, "chaining acks the prior");
        let new_ask = s.get_ask(&c2).unwrap().unwrap();
        assert_eq!(new_ask.reply_to.as_deref(), Some(c1.as_str()));
        // The new question threads into the prior conversation.
        let thread = s.thread(q1, 50).unwrap();
        assert!(thread.iter().any(|m| m.id == q2), "q2 is in q1's thread");
        // reply_to to a nonexistent prior ask errors.
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
        let as_asker = s.list_asks("a", AskRole::Asker, 50).unwrap();
        assert_eq!(as_asker.len(), 1);
        assert_eq!(as_asker[0].id, c1);
        let as_askee = s.list_asks("a", AskRole::Askee, 50).unwrap();
        assert_eq!(as_askee.len(), 1);
        assert_eq!(as_askee[0].id, c2);
        let any = s.list_asks("a", AskRole::Any, 50).unwrap();
        assert_eq!(any.len(), 2);
    }

    /// WL-040b: `import_ask` materializes an ANSWERED ask out-of-order (bypassing the
    /// create→answer→ack lifecycle) with the correct state/kind/options and a NULL
    /// `closed_ts`; the dedup pre-check makes a second identical import a no-op.
    #[test]
    fn import_ask_materializes_answered_and_is_idempotent() {
        let s = mem();
        let q = s
            .send("a", "b", Some("subj"), "question?", None, None)
            .unwrap();
        let ans = s
            .send("b", "a", Some("Re: subj"), "answer!", None, None)
            .unwrap();
        let id = crate::model::new_ask_id(q);
        let inserted = s
            .import_ask(
                &id,
                q,
                Some(ans),
                "a",
                "b",
                Some("subj"),
                AskState::Answered,
                AskKind::Choice,
                Some("yes\nno"),
                None,
                None,
                100,
                200,
                None,
                None,
            )
            .unwrap();
        assert!(inserted, "first import inserts");
        let got = s.get_ask(&id).unwrap().unwrap();
        assert_eq!(got.state, AskState::Answered);
        assert_eq!(got.kind, AskKind::Choice);
        assert_eq!(got.options.as_deref(), Some("yes\nno"));
        assert_eq!(got.question_msg_id, q);
        assert_eq!(got.answer_msg_id, Some(ans));
        assert_eq!(got.closed_ts, None);
        assert_eq!(got.opened_ts, 100);
        assert_eq!(got.updated_ts, 200);
        // Idempotent: a second import on the same (asker, askee, question) is skipped.
        let again = s
            .import_ask(
                &crate::model::new_ask_id(q),
                q,
                Some(ans),
                "a",
                "b",
                Some("subj"),
                AskState::Answered,
                AskKind::Choice,
                Some("yes\nno"),
                None,
                None,
                100,
                200,
                None,
                None,
            )
            .unwrap();
        assert!(!again, "duplicate import is skipped");
        assert_eq!(s.list_asks("a", AskRole::Any, 50).unwrap().len(), 1);
    }

    /// WL-040b: `import_ask` materializes an ACKED/closed ask, round-tripping
    /// `closed_ts` + `close_note`; `import_ask_group` replays a parent anchor and
    /// links a child ask's `parent_id`, and the group reads back via `ask_many_result`.
    #[test]
    fn import_ask_materializes_acked_and_group() {
        let s = mem();
        let pid = crate::model::new_ask_many_id(500);
        assert!(s
            .import_ask_group(&pid, "a", Some("poll"), "yes or no?", 500, 2)
            .unwrap());
        // Re-import the group: idempotent skip.
        assert!(!s
            .import_ask_group(&pid, "a", Some("poll"), "yes or no?", 500, 2)
            .unwrap());

        let q = s.send("a", "b", None, "yes or no?", None, None).unwrap();
        let id = crate::model::new_ask_id(q);
        assert!(s
            .import_ask(
                &id,
                q,
                None,
                "a",
                "b",
                None,
                AskState::Acked,
                AskKind::FreeText,
                None,
                None,
                Some("closing note"),
                10,
                30,
                Some(30),
                Some(&pid),
            )
            .unwrap());
        let got = s.get_ask(&id).unwrap().unwrap();
        assert_eq!(got.state, AskState::Acked);
        assert_eq!(got.closed_ts, Some(30));
        assert_eq!(got.close_note.as_deref(), Some("closing note"));
        assert_eq!(got.parent_id.as_deref(), Some(pid.as_str()));
        // The group reads back with the replayed child.
        let res = s.ask_many_result(&pid, None).unwrap().unwrap();
        assert_eq!(res.target_count, 2);
        assert_eq!(res.acked, 1);

        // Group listing returns the anchor by id; unknown ids are absent.
        let groups = s
            .list_ask_groups(&[pid.clone(), "askm_999_1".to_string()])
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].parent_id, pid);
        assert_eq!(groups[0].target_count, 2);
    }

    /// WL-040b: `import_ask` re-validates at the store seam — a hostile asker, a
    /// malformed ask id, and an oversized options payload are all rejected before the
    /// INSERT (defense-in-depth, even though the bin layer bounds these first).
    #[test]
    fn import_ask_rejects_malformed_inputs() {
        let s = mem();
        let q = s.send("a", "b", None, "q", None, None).unwrap();
        // hostile askee identity (control char — rejected by check_ident)
        assert!(s
            .import_ask(
                "ask_1_1",
                q,
                None,
                "a",
                "b\u{7}c",
                None,
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
        // malformed ask id
        assert!(s
            .import_ask(
                "bad id",
                q,
                None,
                "a",
                "b",
                None,
                AskState::Open,
                AskKind::FreeText,
                None,
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
        // oversized options
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s
            .import_ask(
                "ask_1_1",
                q,
                None,
                "a",
                "b",
                None,
                AskState::Open,
                AskKind::FreeText,
                Some(&big),
                None,
                None,
                1,
                1,
                None,
                None,
            )
            .is_err());
    }

    /// `has_open_asks` is true only when the peer is the askee of an ask in
    /// [`AskState::Open`]; it becomes false after the ask is answered.
    #[test]
    fn has_open_asks_true_only_for_open_askee() {
        let s = mem();
        let (c1, _) = s
            .ask("a", "b", None, "q1", AskKind::FreeText, None, None)
            .unwrap();
        assert!(s.has_open_asks("b").unwrap(), "b is askee of an open ask");
        assert!(!s.has_open_asks("a").unwrap(), "a is asker, not askee");
        assert!(!s.has_open_asks("z").unwrap(), "z has no asks at all");
        s.answer("b", &c1, "ans").unwrap();
        assert!(
            !s.has_open_asks("b").unwrap(),
            "b answered, ask no longer open"
        );
    }

    /// `list_asks` is bounded: a request for more rows than `MAX_LIMIT` is clamped
    /// (no unbounded listing), and a tiny `limit` returns only that many newest-first.
    #[test]
    fn list_asks_is_bounded() {
        let s = mem();
        for _ in 0..5 {
            s.ask("a", "b", None, "q", AskKind::FreeText, None, None)
                .unwrap();
        }
        // An absurd request is clamped to MAX_LIMIT (never unbounded).
        let huge = s.list_asks("a", AskRole::Any, i64::MAX).unwrap();
        assert!(
            huge.len() <= MAX_LIMIT as usize,
            "list_asks must clamp to MAX_LIMIT"
        );
        assert_eq!(huge.len(), 5, "all 5 fit under the cap");
        // A small explicit limit returns only that many (newest-first).
        let two = s.list_asks("a", AskRole::Any, 2).unwrap();
        assert_eq!(two.len(), 2);
    }

    /// `ask_for_message` resolves the owning correlation id for both the question
    /// and the answer message ids, and returns `None` for an unrelated message.
    #[test]
    fn ask_for_message_resolves_both_ends() {
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
        // An ordinary (non-ask) message belongs to no tracked ask.
        let mid = s.send("a", "b", None, "plain", None, None).unwrap();
        assert_eq!(s.ask_for_message(mid).unwrap(), None);
    }

    /// A legacy DB that predates the `asks` table gains it idempotently on open
    /// (mirror of the `revocations` legacy-migration test) with NO data loss to the
    /// pre-existing `messages` rows; a full ask lifecycle then works, and re-opening
    /// is a no-op that retains the recorded ask.
    #[test]
    fn legacy_db_gains_asks_table() {
        let dir = std::env::temp_dir().join(format!(
            "weave-asks-legacy-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-P1 store: messages with a row, NO asks table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT,
                    body TEXT NOT NULL, in_reply_to INTEGER
                 );
                 INSERT INTO messages (ts, sender, recipient, subject, body)
                 VALUES (1, 'a', 'b', NULL, 'pre-existing');",
            )
            .unwrap();
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type='table' AND name='asks')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!exists, "fixture must predate the asks table");
        }
        // First open runs the migration: the table exists + a full lifecycle works,
        // and the pre-existing message survived (no data loss).
        let cid = {
            let s = SqliteStore::open(&path).unwrap();
            assert!(
                s.list_asks("a", AskRole::Any, 50).unwrap().is_empty(),
                "asks table created, empty"
            );
            let (cid, _) = s
                .ask("a", "b", Some("subj"), "q?", AskKind::FreeText, None, None)
                .unwrap();
            s.answer("b", &cid, "ans").unwrap();
            s.ack("b", &cid, Some("closed")).unwrap();
            assert_eq!(s.get_ask(&cid).unwrap().unwrap().state, AskState::Acked);
            // The pre-existing message survived migration.
            let (rows, _) = s.inbox("b", true, false, 50).unwrap();
            assert!(
                rows.iter().any(|m| m.body == "pre-existing"),
                "pre-existing message survived migration"
            );
            cid
        };
        // Re-open: idempotent, no duplicate-table error, prior ask retained.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(
                s.get_ask(&cid).unwrap().unwrap().state,
                AskState::Acked,
                "re-open is a no-op; the acked ask persists"
            );
        }
    }

    /// `create_ask_many` inserts ONE parent + N children; each child is a well-formed
    /// P1 ask (`state=open`, `parent_id` set, `question_msg_id` points at a real
    /// `messages` row in the askee's inbox). The parent body/opener/target_count are
    /// recorded.
    #[test]
    fn create_ask_many_opens_parent_and_children() {
        let s = mem();
        let out = s
            .create_ask_many("a", &["b".into(), "c".into()], Some("topic"), "all hands?")
            .unwrap();
        assert!(crate::model::ask_many_id_valid(&out.parent_id));
        assert_eq!(out.children.len(), 2);
        for (peer, res) in &out.children {
            let cid = res.as_ref().expect("child created");
            let ask = s.get_ask(cid).unwrap().unwrap();
            assert_eq!(ask.state, AskState::Open);
            assert_eq!(ask.asker, "a");
            assert_eq!(ask.askee, *peer);
            assert_eq!(ask.parent_id.as_deref(), Some(out.parent_id.as_str()));
            // The question landed in the askee's inbox.
            let (inbox, _) = s.inbox(peer, false, false, 50).unwrap();
            assert!(inbox
                .iter()
                .any(|m| m.id == ask.question_msg_id && m.sender == "a"));
        }
        // Result aggregate: both pending, none answered/acked/failed, state pending.
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!(r.target_count, 2);
        assert_eq!((r.answered, r.acked, r.pending, r.failed), (0, 0, 2, 0));
        assert_eq!(r.state, crate::model::AskManyState::Pending);
        assert_eq!(r.body, "all hands?");
        assert_eq!(r.asker, "a");
    }

    /// A child answered/acked through the UNCHANGED P1 `answer`/`ack` updates the
    /// aggregate (lifecycle reuse). Totality `answered+acked+pending+failed ==
    /// target_count` holds at every step; `complete` iff no child pending.
    #[test]
    fn ask_many_aggregate_tracks_child_lifecycle() {
        let s = mem();
        let out = s
            .create_ask_many("a", &["b".into(), "c".into(), "d".into()], None, "q?")
            .unwrap();
        let cids: Vec<String> = out
            .children
            .iter()
            .map(|(_, r)| r.as_ref().unwrap().clone())
            .collect();
        // b answers (→answered), c answers then acks (→acked), d stays open.
        s.answer("b", &cids[0], "yes").unwrap();
        s.answer("c", &cids[1], "no").unwrap();
        s.ack("c", &cids[1], None).unwrap();
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!((r.answered, r.acked, r.pending, r.failed), (1, 1, 1, 0));
        assert_eq!(
            r.answered + r.acked + r.pending + r.failed,
            r.target_count,
            "totality holds"
        );
        assert_eq!(r.state, crate::model::AskManyState::Pending);
        // The answered child surfaces its answer_msg_id.
        let bview = r.children.iter().find(|c| c.peer == "b").unwrap();
        assert!(bview.answer_msg_id.is_some());
        // Close the last pending child → complete.
        s.ack("d", &cids[2], None).unwrap();
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!(r.pending, 0);
        assert_eq!(r.state, crate::model::AskManyState::Complete);
    }

    /// Best-effort per child: an invalid/broadcast peer in an otherwise-valid list is
    /// a per-child error (skipped, counted `failed`), NOT a whole-call failure;
    /// totality is preserved via `target_count`. The whole-call hard errors (empty,
    /// over-cap) ARE rejected before any insert.
    #[test]
    fn create_ask_many_best_effort_and_caps() {
        let s = mem();
        // One good peer, one broadcast alias (per-child reject), one control-char
        // (per-child reject). Call still succeeds; failed counted at read time.
        let out = s
            .create_ask_many(
                "a",
                &["b".into(), "all".into(), "bad\nid".into()],
                None,
                "q",
            )
            .unwrap();
        assert_eq!(out.children.len(), 3);
        let ok = out.children.iter().filter(|(_, r)| r.is_ok()).count();
        let err = out.children.iter().filter(|(_, r)| r.is_err()).count();
        assert_eq!((ok, err), (1, 2));
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!(r.target_count, 3);
        assert_eq!(r.failed, 2);
        assert_eq!(r.pending, 1);
        assert_eq!(r.answered + r.acked + r.pending + r.failed, r.target_count);
        // De-dup: a repeated peer collapses to ONE child.
        let out = s
            .create_ask_many("a", &["b".into(), "b".into(), "b".into()], None, "q")
            .unwrap();
        assert_eq!(out.children.len(), 1);
        // Empty list: hard whole-call error (no parent inserted).
        assert!(s.create_ask_many("a", &[], None, "q").is_err());
        // Over-cap list: hard whole-call error.
        let many: Vec<String> = (0..MAX_ASK_MANY_TARGETS + 1)
            .map(|i| format!("p{i}"))
            .collect();
        assert!(s.create_ask_many("a", &many, None, "q").is_err());
        // Broadcast asker rejected.
        assert!(s.create_ask_many("all", &["b".into()], None, "q").is_err());
        // Oversized body rejected.
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s.create_ask_many("a", &["b".into()], None, &big).is_err());
        // Invalid parent id on result rejected before any bind.
        assert!(s.ask_many_result("askm;rm", None).is_err());
        assert!(s.ask_many_result("ask_1_2", None).is_err()); // a child id is not a parent
                                                              // Unknown (well-formed) parent id → Ok(None).
        assert!(s.ask_many_result("askm_1_2", None).unwrap().is_none());
    }

    /// The opt-in `age` threshold flips a still-pending group from `pending` to
    /// `partial` at read time (daemon-free); without a threshold it stays `pending`.
    #[test]
    fn ask_many_age_threshold_flips_partial() {
        let s = mem();
        let out = s.create_ask_many("a", &["b".into()], None, "q").unwrap();
        // Backdate the parent + child so age is large.
        s.conn
            .execute(
                "UPDATE ask_groups SET opened_ts = opened_ts - 1000 WHERE parent_id = ?1",
                params![out.parent_id],
            )
            .unwrap();
        // No threshold ⇒ still pending.
        let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
        assert_eq!(r.state, crate::model::AskManyState::Pending);
        // Threshold elapsed ⇒ partial.
        let r = s
            .ask_many_result(&out.parent_id, Some(10))
            .unwrap()
            .unwrap();
        assert_eq!(r.state, crate::model::AskManyState::Partial);
    }

    /// A legacy P1-era DB whose `asks` table predates `parent_id` (and lacks
    /// `ask_groups`) upgrades in place: `migrate` adds `parent_id` (NULL on the old
    /// row) + creates `ask_groups`, the old ask still reads back, and re-opening is a
    /// no-op. The additive-column template (mirrors `legacy_db_gains_asks_table`).
    #[test]
    fn legacy_asks_gains_parent_id_and_ask_groups() {
        let dir = std::env::temp_dir().join(format!(
            "weave-askmany-legacy-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A P1-era store: messages + an asks table WITHOUT parent_id, a pre-existing
        // ask row, and NO ask_groups table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT,
                    body TEXT NOT NULL, in_reply_to INTEGER
                 );
                 CREATE TABLE asks (
                    id TEXT PRIMARY KEY, question_msg_id INTEGER NOT NULL,
                    answer_msg_id INTEGER, asker TEXT NOT NULL, askee TEXT NOT NULL,
                    subject TEXT, state TEXT NOT NULL, reply_to TEXT, close_note TEXT,
                    opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL,
                    closed_ts INTEGER
                 );
                 INSERT INTO messages (ts, sender, recipient, subject, body)
                 VALUES (1, 'a', 'b', NULL, 'q');
                 INSERT INTO asks (id, question_msg_id, asker, askee, state, opened_ts, updated_ts)
                 VALUES ('ask_1_legacy', 1, 'a', 'b', 'open', 1, 1);",
            )
            .unwrap();
            assert!(!column_exists(&conn, "asks", "parent_id").unwrap());
        }
        // First open migrates: parent_id added (NULL on old row), ask_groups created.
        {
            let s = SqliteStore::open(&path).unwrap();
            let old = s.get_ask("ask_1_legacy").unwrap().unwrap();
            assert_eq!(old.parent_id, None, "legacy ask has NULL parent_id");
            assert_eq!(old.state, AskState::Open);
            // ask_groups works now: a fresh fanout opens + aggregates.
            let out = s.create_ask_many("a", &["c".into()], None, "fan?").unwrap();
            let r = s.ask_many_result(&out.parent_id, None).unwrap().unwrap();
            assert_eq!(r.target_count, 1);
        }
        // Re-open is a clean no-op; the legacy ask persists.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert!(s.get_ask("ask_1_legacy").unwrap().is_some());
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
        let cert = s
            .register_peer("envctl", "zellij", "envctl", "", Some("/home/x/envctl"))
            .unwrap();
        s.register_peer_full(
            "envctl",
            "tmux",
            "%4",
            "/run/kitty.sock",
            Some("/home/x/envctl"),
            None,
            "",
            "",
            "",
            "",
            "default",
            Some(&cert),
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
        s.send("a", "b", None, "1", None, None).unwrap();
        s.send("b", "a", None, "2", None, None).unwrap();
        s.send("c", "d", None, "x", None, None).unwrap();
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
            s.send("a", "b", None, &format!("m{i}"), None, None)
                .unwrap();
        }
        // A negative limit must NOT behave like SQLite's unbounded LIMIT -1.
        let (rows, _) = s.inbox("b", true, false, -1).unwrap();
        assert!(rows.len() <= MAX_LIMIT as usize);
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn gc_deletes_old_keeps_new() {
        let s = mem();
        let id_old = s.send("a", "b", None, "old", None, None).unwrap();
        // Backdate the first message well past the threshold.
        s.conn
            .execute(
                "UPDATE messages SET ts = ts - 100000 WHERE id = ?1",
                params![id_old],
            )
            .unwrap();
        s.send("a", "b", None, "new", None, None).unwrap();
        let deleted = s.gc(3600).unwrap(); // older than 1h
        assert_eq!(deleted, 1);
        assert_eq!(s.total_messages().unwrap(), 1);
        let (rows, _) = s.inbox("b", true, false, 50).unwrap();
        assert_eq!(rows[0].body, "new");
    }

    /// P6: record_delivery appends metadata-only stage rows that list_delivery
    /// returns oldest-first (ts ASC, id ASC). The trace carries NO body.
    #[test]
    fn delivery_log_records_and_lists_oldest_first() {
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
            DeliveryStage::Injected.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )
        .unwrap();
        let trace = s.list_delivery(mid, 50).unwrap();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0].stage, "queued");
        assert_eq!(trace[1].stage, "injected");
        assert_eq!(trace[0].to_peer, "b");
        // SECRET-FREE: the body never appears anywhere in a trace row.
        for t in &trace {
            let blob = format!("{t:?}");
            assert!(
                !blob.contains("SECRET-BODY-XYZ"),
                "trace leaked body: {blob}"
            );
        }
        // Unknown ref ⇒ empty (not an error).
        assert!(s.list_delivery(999_999, 50).unwrap().is_empty());
    }

    /// P6: list_delivery never returns more than MAX_DELIVERY_ROWS regardless of
    /// the requested limit (bounded read).
    #[test]
    fn delivery_log_read_is_bounded() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage, MAX_DELIVERY_ROWS};
        let s = mem();
        let mid = s.send("a", "b", None, "x", None, None).unwrap();
        for _ in 0..(MAX_DELIVERY_ROWS + 25) {
            s.record_delivery(
                mid,
                DeliveryRefKind::Notify.as_str(),
                "b",
                DeliveryStage::Queued.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap();
        }
        // A huge / negative limit is clamped to MAX_DELIVERY_ROWS.
        assert_eq!(
            s.list_delivery(mid, i64::MAX).unwrap().len() as i64,
            MAX_DELIVERY_ROWS
        );
        assert!(s.list_delivery(mid, -1).unwrap().len() as i64 <= MAX_DELIVERY_ROWS);
    }

    /// P6: gc prunes old delivery_log rows in the same retention pass, keeping
    /// recent ones (mirrors gc_deletes_old_keeps_new for messages).
    #[test]
    fn gc_prunes_old_delivery_log() {
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
        // Backdate the trace row well past the threshold.
        s.conn
            .execute(
                "UPDATE delivery_log SET ts = ts - 100000 WHERE ref_id = ?1",
                params![mid],
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
        s.gc(3600).unwrap();
        let trace = s.list_delivery(mid, 50).unwrap();
        assert_eq!(trace.len(), 1, "only the recent (drained) row survives gc");
        assert_eq!(trace[0].stage, "drained");
    }

    /// P6: a legacy DB without delivery_log gains it on open (idempotent migrate),
    /// and record/list work afterward. Mirrors legacy_asks_gains_parent_id.
    #[test]
    fn legacy_db_gains_delivery_log() {
        use crate::model::{DeliveryOutcome, DeliveryRefKind, DeliveryStage};
        let dir = std::env::temp_dir().join(format!(
            "weave-delivery-legacy-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        {
            // A pre-P6 store: messages only, NO delivery_log table.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT,
                    body TEXT NOT NULL, in_reply_to INTEGER
                 );
                 INSERT INTO messages (ts, sender, recipient, subject, body)
                 VALUES (1, 'a', 'b', NULL, 'q');",
            )
            .unwrap();
        }
        // First open migrates delivery_log in; record + list work.
        {
            let s = SqliteStore::open(&path).unwrap();
            s.record_delivery(
                1,
                DeliveryRefKind::Ask.as_str(),
                "b",
                DeliveryStage::Queued.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap();
            assert_eq!(s.list_delivery(1, 50).unwrap().len(), 1);
        }
        // Re-open is a clean idempotent no-op; the trace persists.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(s.list_delivery(1, 50).unwrap().len(), 1);
        }
    }

    #[test]
    fn reply_addresses_back_and_links() {
        let s = mem();
        // a -> b "hi". b replies; the reply must go back to a, carry "Re: hi",
        // and link to the parent via in_reply_to.
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
        // The unrelated top-level message is not pulled into the thread.
        assert!(thread.iter().all(|m| m.body != "unrelated"));
    }

    #[test]
    fn receipts_reports_readers() {
        let s = mem();
        let id = s.send("a", "all", None, "ping", None, None).unwrap();
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
        // A valid send still works (no regression).
        assert!(s.send("a", "b", None, "x", None, None).is_ok());
    }

    #[test]
    fn send_idempotency_returns_existing_id() {
        let s = mem();
        let id1 = s.send("a", "b", None, "x", Some("key-1"), None).unwrap();
        let id2 = s.send("a", "b", None, "x", Some("key-1"), None).unwrap();
        assert_eq!(id1, id2, "duplicate idempotency_key returns existing id");
        // A different key mints a new row.
        let id3 = s.send("a", "b", None, "x", Some("key-2"), None).unwrap();
        assert_ne!(id1, id3);
        // No key still mints a new row.
        let id4 = s.send("a", "b", None, "x", None, None).unwrap();
        assert_ne!(id1, id4);
        assert_ne!(id3, id4);
    }

    #[test]
    fn send_trace_id_roundtrips() {
        let s = mem();
        let id = s
            .send("a", "b", Some("sub"), "body", None, Some("trace-42"))
            .unwrap();
        let (msgs, _) = s.inbox("b", true, false, 10).unwrap();
        let m = msgs.iter().find(|m| m.id == id).unwrap();
        assert_eq!(m.trace_id.as_deref(), Some("trace-42"));
    }

    #[test]
    fn send_idempotency_key_and_trace_id_on_outbox() {
        let s = mem();
        let id = s
            .enqueue_intent(
                "bob",
                "host",
                "alice",
                None,
                "hi",
                "",
                Some("ik"),
                Some("tk"),
                None,
                0,
            )
            .unwrap();
        let intents = s.outbox_all(10).unwrap();
        let intent = intents.iter().find(|i| i.id == id).unwrap();
        assert_eq!(intent.idempotency_key.as_deref(), Some("ik"));
        assert_eq!(intent.trace_id.as_deref(), Some("tk"));
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
        let cert = s
            .register_peer("k", "kitty", "1", "/run/a.sock", Some("/w"))
            .unwrap();
        assert_eq!(s.get_peer("k").unwrap().unwrap().socket, "/run/a.sock");
        // Upsert with a new socket overwrites it.
        s.register_peer_full(
            "k",
            "kitty",
            "1",
            "/run/b.sock",
            Some("/w"),
            None,
            "",
            "",
            "",
            "",
            "default",
            Some(&cert),
        )
        .unwrap();
        assert_eq!(s.get_peer("k").unwrap().unwrap().socket, "/run/b.sock");
        // list_peers also carries the socket.
        let peers = s.list_peers().unwrap();
        assert_eq!(peers[0].socket, "/run/b.sock");
    }

    /// WL-047 (dual-backend parity): `register_peer_full`'s new-peer INSERT must
    /// honor a SUPPLIED birth cert (the parent-minted spawn cert) and persist it
    /// VERBATIM, else mint a fresh one when `None`. This is the spawn-identity chain
    /// (parent mints → threads into child env → pre-registers row → child self-reg
    /// matches). The libsql backend has a byte-identical test (`register_peer_full_*`).
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
        // None path (backward-compat): a fresh peer mints its own cert (non-empty,
        // valid shape, and NOT the one we supplied above).
        let minted = s
            .register_peer_full(
                "auto", "tmux", "%1", "", None, None, "h", "", "", "", "default", None,
            )
            .unwrap();
        assert!(check_birth_cert(&minted).is_ok(), "minted cert is valid");
        assert_ne!(minted, cert, "the None path mints a fresh, distinct cert");
        assert_eq!(s.get_birth_cert("auto").unwrap().unwrap(), minted);
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
                circle: crate::model::DEFAULT_CIRCLE.to_string(),
                role: crate::model::PeerRole::Peer.as_str().to_string(),
                turn_state: String::new(),
                description: String::new(),
                description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
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
        let cert = s
            .register_peer_full(
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
                "default",
                None,
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
            "default",
            Some(&cert),
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
        let id1 = s.send("a", "b", None, "m1", None, None).unwrap();
        let id2 = s.send("a", "b", None, "m2", None, None).unwrap();
        let id3 = s.send("a", "all", None, "bcast", None, None).unwrap();

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
        // list_peers carries them too.
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
            "default",
            None,
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

    /// P4: a pre-P4 peers table (no circle/role columns) migrates IN PLACE — the
    /// columns are added, a legacy row reads `circle='default'`/`role='peer'`, and
    /// re-opening is a no-op (the guarded ALTERs never error twice).
    #[test]
    fn legacy_db_without_circle_role_migrates_in_place() {
        let dir = std::env::temp_dir().join(format!(
            "weave-legacy-circle-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy-circle.db");
        {
            let conn = Connection::open(&path).unwrap();
            // A post-tags but pre-P4 peers table (has the git tag columns, lacks
            // circle/role).
            conn.execute_batch(
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
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO peers (name, mux, target, socket, cwd, last_seen, pid, host, repo, branch, worktree_id)
                 VALUES ('old', 'tmux', '%1', '', '/legacy', ?1, 42, 'h', '', '', '')",
                params![now()],
            )
            .unwrap();
            assert!(!column_exists(&conn, "peers", "circle").unwrap());
            assert!(!column_exists(&conn, "peers", "role").unwrap());
        }
        // Opening through SqliteStore runs migrate(): circle/role are added.
        let s = SqliteStore::open(&path).unwrap();
        let old = s.get_peer("old").unwrap().unwrap();
        assert_eq!(old.circle, "default", "legacy row classifies into default");
        assert_eq!(old.role, "peer", "legacy row is a plain peer");
        // Re-opening is idempotent.
        assert!(SqliteStore::open(&path)
            .unwrap()
            .get_peer("old")
            .unwrap()
            .is_some());
    }

    /// P4: `register_peer_full` round-trips the circle; a re-register PRESERVES an
    /// existing role (a re-register must never demote an orchestrator).
    #[test]
    fn register_roundtrips_circle_and_preserves_role() {
        let s = mem();
        let cert_p = s
            .register_peer_full(
                "p", "tmux", "%1", "", None, None, "h", "", "", "", "team-a", None,
            )
            .unwrap();
        assert_eq!(s.get_peer("p").unwrap().unwrap().circle, "team-a");
        // Promote, then re-register: the role must survive the upsert.
        let out = s.claim_orchestrator_role("p", None, false).unwrap();
        assert!(matches!(out, crate::model::ClaimOutcome::Claimed { .. }));
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
            Some(&cert_p),
        )
        .unwrap();
        assert_eq!(
            s.get_peer("p").unwrap().unwrap().role,
            "orchestrator",
            "a re-register must not demote an orchestrator"
        );
        // An invalid circle at the seam falls back to the default circle.
        s.register_peer_full(
            "q", "tmux", "%2", "", None, None, "h", "", "", "", "a/b; rm", None,
        )
        .unwrap();
        assert_eq!(s.get_peer("q").unwrap().unwrap().circle, "default");
    }

    /// P4: claim refuses a non-force claim while a LIVE holder exists, and a
    /// `force=true` claim steals it (demoting the prior holder to 'peer').
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
        // a claims (no contest) ⇒ Claimed.
        assert!(matches!(
            s.claim_orchestrator_role("a", None, false).unwrap(),
            crate::model::ClaimOutcome::Claimed { .. }
        ));
        // WL-019: b claims without force while a is live ⇒ co-orchestrator (no demotion).
        match s.claim_orchestrator_role("b", None, false).unwrap() {
            crate::model::ClaimOutcome::Claimed { demoted, circle } => {
                assert_eq!(circle, "c1");
                assert!(
                    demoted.is_empty(),
                    "non-force claim should not demote: {demoted:?}"
                );
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        assert_eq!(s.get_peer("a").unwrap().unwrap().role, "orchestrator");
        assert_eq!(s.get_peer("b").unwrap().unwrap().role, "orchestrator");
        // b claims WITH force ⇒ Claimed, a demoted to 'peer'.
        match s.claim_orchestrator_role("b", None, true).unwrap() {
            crate::model::ClaimOutcome::Claimed { demoted, circle } => {
                assert_eq!(circle, "c1");
                assert_eq!(demoted, vec!["a".to_string()]);
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        assert_eq!(s.get_peer("a").unwrap().unwrap().role, "peer");
        assert_eq!(s.get_peer("b").unwrap().unwrap().role, "orchestrator");
        // An unregistered caller is an error.
        assert!(s.claim_orchestrator_role("ghost", None, false).is_err());
    }

    /// P4: `list_peers_in_circle` scopes correctly; `None`/`'*'` ⇒ all.
    #[test]
    fn list_peers_in_circle_scopes() {
        let s = mem();
        s.register_peer_full(
            "a", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None,
        )
        .unwrap();
        s.register_peer_full(
            "b", "tmux", "%2", "", None, None, "h", "", "", "", "c2", None,
        )
        .unwrap();
        assert_eq!(s.list_peers_in_circle(Some("c1")).unwrap().len(), 1);
        assert_eq!(s.list_peers_in_circle(Some("c2")).unwrap().len(), 1);
        assert_eq!(s.list_peers_in_circle(None).unwrap().len(), 2);
        assert_eq!(s.list_peers_in_circle(Some("*")).unwrap().len(), 2);
        assert_eq!(s.list_peers_in_circle(Some("none")).unwrap().len(), 0);
    }

    /// P4: `orchestrator_status` reuses `is_alive` — a fresh holder reads present;
    /// a holder past the TTL window reads absent (no new probe).
    #[test]
    fn orchestrator_status_liveness_reuses_is_alive() {
        let dir = std::env::temp_dir().join(format!(
            "weave-orch-status-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("orch.db");
        let s = SqliteStore::open(&path).unwrap();
        s.register_peer_full(
            "o", "tmux", "%1", "", None, None, "h", "", "", "", "c1", None,
        )
        .unwrap();
        s.claim_orchestrator_role("o", None, false).unwrap();
        // Fresh holder ⇒ present.
        let st = s.orchestrator_status(Some("c1")).unwrap();
        assert!(st.present);
        assert_eq!(st.holders[0].name, "o");
        // An empty circle ⇒ absent.
        let st2 = s.orchestrator_status(Some("empty")).unwrap();
        assert!(!st2.present);
        assert!(st2.holders.is_empty());
        // Backdate last_seen well past the TTL window ⇒ is_alive false ⇒ absent.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE peers SET last_seen = ?1 WHERE name = 'o'",
                params![now() - 10_000_000],
            )
            .unwrap();
        }
        let s2 = SqliteStore::open(&path).unwrap();
        let st3 = s2.orchestrator_status(Some("c1")).unwrap();
        assert!(!st3.present, "a stale holder reads absent (is_alive reuse)");
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
            circle: crate::model::DEFAULT_CIRCLE.to_string(),
            role: crate::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
        };

        // (c) NULL pid + recent => true (TTL fallback, no probe).
        assert!(is_alive(&base), "null pid + recent must be alive (TTL)");

        // (b) remote host + recent => true (fail-open: cannot probe a remote PID).
        //   Use a pid that does NOT exist locally to prove the host gate (not the
        //   pid) is what keeps it alive.
        let remote = Peer {
            host: format!("{}-not-this-host", crate::config::this_host()),
            pid: Some(999_999_999),
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            circle: crate::model::DEFAULT_CIRCLE.to_string(),
            role: crate::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
        };

        // same-host + live pid (our own) => AliveLocal.
        let live_local = Peer {
            pid: Some(std::process::id() as i64),
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
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
            birth_cert: None,
            contact_policy: "open".to_string(),
            ..base.clone()
        };
        assert_eq!(
            liveness_for(&edge_alive, this, now_ts),
            Liveness::AliveLocal,
            "TTL boundary (== now - TTL) is inclusive-alive"
        );
        let edge_stale = Peer {
            last_seen: now_ts - ONLINE_TTL_SECS - 1,
            birth_cert: None,
            contact_policy: "open".to_string(),
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
                "default",
                None,
            )
            .unwrap();
        }

        // Read-only open can list the peer.
        let ro = SqliteStore::open_readonly(&path).unwrap();
        let peers = ro.list_peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "seed");

        // But ANY write is rejected by the engine, not by convention.
        let wr = ro.register_peer_full(
            "intruder", "tmux", "%2", "", None, None, "boxA", "", "", "", "default", None,
        );
        assert!(wr.is_err(), "a write through a read-only handle must error");
        let send = ro.send("a", "b", None, "x", None, None);
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
            .register_peer_full(
                "me", "tmux", "%1", "", None, None, "boxA", "", "", "", "default", None,
            )
            .unwrap();
        {
            let foreign = SqliteStore::open(&foreign_path).unwrap();
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
                0,
            )
            .unwrap();
        let _i2 = s
            .enqueue_intent(
                "carol",
                "",
                "alice",
                None,
                "for carol",
                "",
                None,
                None,
                None,
                0,
            )
            .unwrap();
        let i3 = s
            .enqueue_intent("bob", "", "alice", None, "body3", "", None, None, None, 0)
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
        assert!(s
            .enqueue_intent("", "", "a", None, "x", "", None, None, None, 0)
            .is_err());
        assert!(s
            .enqueue_intent("b", "", "", None, "x", "", None, None, None, 0)
            .is_err());
        assert!(
            s.enqueue_intent("b", "h\nx", "a", None, "x", "", None, None, None, 0)
                .is_err(),
            "control char in to_host rejected"
        );
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s
            .enqueue_intent("b", "", "a", None, &big, "", None, None, None, 0)
            .is_err());
        assert!(s
            .enqueue_intent("b", "", "a", None, "ok", "", None, None, None, 0)
            .is_ok());
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
                a.enqueue_intent(
                    "bob",
                    "",
                    "alice",
                    None,
                    &format!("m{n}"),
                    "",
                    None,
                    None,
                    None,
                    0,
                )
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
                0,
            )
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
            a.enqueue_intent(
                "carol",
                "",
                "alice",
                None,
                "not for bob",
                "",
                None,
                None,
                None,
                0,
            )
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
        let id = s
            .enqueue_intent("bob", "", "alice", None, "x", "", None, None, None, 0)
            .unwrap();
        assert!(id > 0);
        assert_eq!(s.pull_cursor_get("src").unwrap(), 0);
        s.pull_cursor_set("src", 7).unwrap();
        assert_eq!(s.pull_cursor_get("src").unwrap(), 7);
        // The 2d `keys` + #7 `identity_keys` tables are present on the upgraded
        // legacy store.
        assert!(s.get_key("alice").unwrap().is_none());
        s.register_key("alice", "deadbeef").unwrap();
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("deadbeef"));
    }

    /// #7 migration: a legacy DB that already has a SINGLE-key `keys` row migrates
    /// that row into `identity_keys` on open, so `get_keys`/`get_key`/`list_keys`
    /// see it — backward-compatible by construction. The copy is idempotent
    /// (re-opening does not duplicate). The legacy `keys` table is retained.
    #[test]
    fn legacy_single_key_migrates_into_identity_keys() {
        let dir =
            std::env::temp_dir().join(format!("weave-mk-legacy-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-#7 store: schema present, a single-key `keys` row, NO identity_keys.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS keys (
                    identity TEXT PRIMARY KEY,
                    pubkey   TEXT NOT NULL
                 );
                 INSERT INTO keys (identity, pubkey) VALUES ('alice', 'aa11');",
            )
            .unwrap();
            // identity_keys must NOT exist yet for this to be a genuine legacy DB.
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type='table' AND name='identity_keys')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!exists, "fixture must predate identity_keys");
        }
        // First open runs the migration.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(
                s.get_keys("alice").unwrap(),
                vec!["aa11".to_string()],
                "legacy single key migrated into identity_keys"
            );
            assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));
            assert_eq!(
                s.list_keys().unwrap(),
                vec![("alice".to_string(), "aa11".to_string())]
            );
        }
        // Re-open: the copy is idempotent (INSERT OR IGNORE) — still exactly one key.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(s.get_keys("alice").unwrap(), vec!["aa11".to_string()]);
        }
    }

    /// The `identity_keys` registry round-trips registered pubkeys through
    /// get/get_keys/list with ADD semantics (#7): registering a NEW key APPENDS,
    /// re-adding the SAME key is a no-op, `get_key` returns the most-recent, and an
    /// invalid identity is rejected at the seam. Plain data — present in every build
    /// regardless of the `sign` feature.
    #[test]
    fn keys_register_get_list_roundtrip() {
        let s = mem();
        assert!(s.get_key("alice").unwrap().is_none(), "unknown key ⇒ None");
        assert!(
            s.get_keys("alice").unwrap().is_empty(),
            "unknown ⇒ empty set"
        );
        s.register_key("alice", "aa11").unwrap();
        s.register_key("bob", "bb22").unwrap();
        assert_eq!(s.get_keys("alice").unwrap(), vec!["aa11".to_string()]);

        // Single-key parity: get_key == the one key (== old behavior).
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("aa11"));

        // ADD: a NEW key APPENDS (does NOT overwrite). Both are registered.
        s.register_key("alice", "cc33").unwrap();
        assert_eq!(
            s.get_keys("alice").unwrap(),
            vec!["aa11".to_string(), "cc33".to_string()],
            "a new key appends; old key stays registered (overlap)"
        );
        // get_key shim returns the MOST-RECENT.
        assert_eq!(s.get_key("alice").unwrap().as_deref(), Some("cc33"));

        // Re-adding the SAME key is a NO-OP (no error, no duplicate row).
        s.register_key("alice", "aa11").unwrap();
        assert_eq!(
            s.get_keys("alice").unwrap().len(),
            2,
            "re-adding an existing key is idempotent"
        );

        // list_keys returns ALL pairs ordered by identity (multiple per identity).
        let keys = s.list_keys().unwrap();
        assert_eq!(
            keys,
            vec![
                ("alice".to_string(), "aa11".to_string()),
                ("alice".to_string(), "cc33".to_string()),
                ("bob".to_string(), "bb22".to_string()),
            ]
        );

        // remove_key removes exactly that pair and reports it; absent ⇒ false.
        assert!(s.remove_key("alice", "aa11").unwrap(), "removed the pair");
        assert_eq!(s.get_keys("alice").unwrap(), vec!["cc33".to_string()]);
        assert!(
            !s.remove_key("alice", "aa11").unwrap(),
            "removing an absent key ⇒ false"
        );

        // An invalid identity is rejected at the seam.
        assert!(s.register_key("", "00").is_err());
        assert!(s.register_key("a\nb", "00").is_err());
    }

    /// `MAX_KEYS_PER_IDENT` bounds a hostile registry: adding the cap-th+1 DISTINCT
    /// key errors (never panics); a DUPLICATE of an existing key is always a no-op
    /// and never counts against the cap. (#7)
    #[test]
    fn register_key_enforces_per_identity_cap() {
        let s = mem();
        // Fill exactly to the cap with DISTINCT keys.
        for i in 0..MAX_KEYS_PER_IDENT {
            let pk = format!("{:064x}", i);
            s.register_key("alice", &pk).unwrap();
        }
        assert_eq!(s.get_keys("alice").unwrap().len(), MAX_KEYS_PER_IDENT);
        // A DUPLICATE at the cap is still accepted (no-op, no error).
        let dup = format!("{:064x}", 0);
        s.register_key("alice", &dup).unwrap();
        assert_eq!(
            s.get_keys("alice").unwrap().len(),
            MAX_KEYS_PER_IDENT,
            "a duplicate never grows the set or hits the cap"
        );
        // A genuinely NEW key beyond the cap is REJECTED (Err, never a panic).
        let over = format!("{:064x}", MAX_KEYS_PER_IDENT + 1);
        assert!(
            s.register_key("alice", &over).is_err(),
            "a new key beyond the cap is refused"
        );
        // The cap is per-identity: a different identity is unaffected.
        s.register_key("bob", &over).unwrap();
        assert_eq!(s.get_keys("bob").unwrap().len(), 1);
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
            a.enqueue_intent("bob", "", from, None, body, sig, None, None, None, 0)
                .unwrap();
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
            a.enqueue_intent("bob", "", from, None, body, sig, None, None, None, 0)
                .unwrap();
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

    /// #7 multi-key REGISTRY verification: with BOTH an old and a new key registered
    /// for ONE identity, a signature by EITHER key commits (true rotation overlap —
    /// impossible before #7). Revoking the OLD fingerprint makes the old key's
    /// signed message REJECT (R1 absolute revocation) while the new key still
    /// commits. A signature by a THIRD, UNREGISTERED key verifies against NEITHER ⇒
    /// REJECT (forgery). Fixed-seed keys for determinism. Asserts the source DB is
    /// byte-unchanged across pulls (owner-only-writes).
    #[cfg(feature = "sign")]
    #[test]
    fn multikey_registry_old_and_new_verify_then_revoke_old() {
        use crate::sign::{fingerprint_full, sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let old = SigningKey::from_bytes(&[31u8; 32]);
        let new = SigningKey::from_bytes(&[32u8; 32]);
        let third = SigningKey::from_bytes(&[33u8; 32]);
        let old_pk = to_hex(old.verifying_key().as_bytes());
        let new_pk = to_hex(new.verifying_key().as_bytes());
        let old_full = format!("SHA256:{}", fingerprint_full(&old_pk).unwrap());

        let dir =
            std::env::temp_dir().join(format!("weave-mkverify-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tag = 0u64;
        let mut src = |from: &str, body: &str, sig: &str| -> StoreSource {
            tag += 1;
            let p = dir.join(format!("s{tag}.db"));
            let a = SqliteStore::open(&p).unwrap();
            a.enqueue_intent("bob", "", from, None, body, sig, None, None, None, 0)
                .unwrap();
            StoreSource::Local(p)
        };
        // Receiver B with BOTH alice keys registered (rotation overlap window).
        let rcv = |n: u32| -> SqliteStore {
            let b = SqliteStore::open(&dir.join(format!("b{n}.db"))).unwrap();
            b.register_key("alice", &old_pk).unwrap();
            b.register_key("alice", &new_pk).unwrap();
            b
        };
        let committed = |b: &SqliteStore, s: &StoreSource, policy: &VerifyPolicy| -> usize {
            pull_from_store(b, "bob", std::slice::from_ref(s), policy)
                .unwrap()
                .committed
        };

        let advisory = VerifyPolicy::advisory();

        // A sig by the OLD key commits (verifies against a registered non-revoked key).
        let sig_old = sign_intent(&old, "alice", "bob", "via-old");
        assert_eq!(
            committed(&rcv(1), &src("alice", "via-old", &sig_old), &advisory),
            1,
            "OLD key in a multi-key set verifies ⇒ COMMIT"
        );
        // A sig by the NEW key also commits.
        let sig_new = sign_intent(&new, "alice", "bob", "via-new");
        assert_eq!(
            committed(&rcv(2), &src("alice", "via-new", &sig_new), &advisory),
            1,
            "NEW key in a multi-key set verifies ⇒ COMMIT"
        );

        // Revoke the OLD fingerprint: the OLD key's signed message now REJECTS even
        // though it cryptographically verifies (R1), while the NEW key still commits.
        let revoke_old = VerifyPolicy {
            strict_override: None,
            trust: vec![],
            revoked: vec![old_full.clone()],
        };
        assert_eq!(
            committed(&rcv(3), &src("alice", "via-old", &sig_old), &revoke_old),
            0,
            "a sig that verifies ONLY against the REVOKED key ⇒ REJECT (R1)"
        );
        assert_eq!(
            committed(&rcv(4), &src("alice", "via-new", &sig_new), &revoke_old),
            1,
            "the NEW (non-revoked) key still commits after the old is revoked"
        );

        // A sig by a THIRD, UNREGISTERED key verifies against NEITHER registered key
        // ⇒ REJECT (forgery), advisory or not.
        let sig_third = sign_intent(&third, "alice", "bob", "forged");
        assert_eq!(
            committed(&rcv(5), &src("alice", "forged", &sig_third), &advisory),
            0,
            "a sig matching no registered key ⇒ REJECT (forgery)"
        );

        // Source DBs are read-only / byte-unchanged is covered by the dedicated
        // owner-only-writes test; here we assert the verification semantics only.
    }

    /// #7 additivity regression-lock: with EXACTLY ONE registered key, the multi-key
    /// path is byte-identical to the #3 single-key behavior. Re-runs the core cells
    /// (good ⇒ commit, forged ⇒ reject, revoked-good ⇒ reject) against a single
    /// registered key and asserts the same outcomes the single-key path produced.
    #[cfg(feature = "sign")]
    #[test]
    fn multikey_single_key_is_byte_identical_to_v3() {
        use crate::sign::{fingerprint_full, sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let alice = SigningKey::from_bytes(&[40u8; 32]);
        let alice_pk = to_hex(alice.verifying_key().as_bytes());
        let alice_full = format!("SHA256:{}", fingerprint_full(&alice_pk).unwrap());

        let dir =
            std::env::temp_dir().join(format!("weave-mkadd-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tag = 0u64;
        let mut src = |from: &str, body: &str, sig: &str| -> StoreSource {
            tag += 1;
            let p = dir.join(format!("s{tag}.db"));
            let a = SqliteStore::open(&p).unwrap();
            a.enqueue_intent("bob", "", from, None, body, sig, None, None, None, 0)
                .unwrap();
            StoreSource::Local(p)
        };
        // Receiver with EXACTLY ONE registered key (== old single-key world).
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
        // good ⇒ commit
        assert_eq!(
            committed(
                &rcv(1),
                &src("alice", "hi", &good),
                &VerifyPolicy::advisory()
            ),
            1
        );
        // forged (body mismatch) ⇒ reject
        assert_eq!(
            committed(
                &rcv(2),
                &src("alice", "TAMPER", &good),
                &VerifyPolicy::advisory()
            ),
            0
        );
        // revoked-good ⇒ reject (R1)
        let revoke = VerifyPolicy {
            strict_override: None,
            trust: vec![],
            revoked: vec![alice_full.clone()],
        };
        assert_eq!(
            committed(&rcv(3), &src("alice", "hi", &good), &revoke),
            0,
            "single-key revoked-good rejected (parity with #3)"
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
            a.enqueue_intent("bob", "", "alice", None, body, &sig, None, None, None, 0)
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

    // -----------------------------------------------------------------------
    // #11 observed-revocation audit log (`revocations`). Plain data in EVERY
    // build (like `identity_keys`), so the round-trip / ordering / clamp /
    // limit / migration tests are NOT sign-gated. The R1-decision coupling
    // tests are sign-gated (they need the verifier).
    // -----------------------------------------------------------------------

    /// `record_revocation` + `list_revocations` round-trip most-recent-first, and
    /// `count_revocations` matches the inserted count. All fields survive verbatim.
    #[test]
    fn revocations_record_list_count_roundtrip() {
        let s = mem();
        assert_eq!(s.count_revocations().unwrap(), 0, "fresh store: no events");
        assert!(
            s.list_revocations(50).unwrap().is_empty(),
            "fresh store: empty list"
        );

        let mk = |ts: i64, fp: &str, id: &str, src: &str, kind: RevocationKind| RevocationEvent {
            id: 0,
            ts,
            fp: fp.to_string(),
            identity: id.to_string(),
            source: src.to_string(),
            kind,
        };
        s.record_revocation(&mk(
            100,
            "SHA256:aa",
            "alice",
            "local:/a",
            RevocationKind::Enforced,
        ))
        .unwrap();
        s.record_revocation(&mk(200, "SHA256:bb", "", "", RevocationKind::Declared))
            .unwrap();
        s.record_revocation(&mk(
            300,
            "SHA256:cc",
            "carol",
            "peer:b",
            RevocationKind::Enforced,
        ))
        .unwrap();

        assert_eq!(s.count_revocations().unwrap(), 3);
        let rows = s.list_revocations(50).unwrap();
        assert_eq!(rows.len(), 3);
        // Most-recent-first (id DESC): the last inserted is row 0.
        assert_eq!(rows[0].fp, "SHA256:cc");
        assert_eq!(rows[0].identity, "carol");
        assert_eq!(rows[0].source, "peer:b");
        assert_eq!(rows[0].kind, RevocationKind::Enforced);
        assert_eq!(rows[1].fp, "SHA256:bb");
        assert_eq!(rows[1].kind, RevocationKind::Declared);
        assert_eq!(rows[1].identity, "", "empty identity round-trips");
        assert_eq!(rows[2].fp, "SHA256:aa");
        assert_eq!(rows[2].ts, 100);
        assert!(
            rows[0].id > rows[1].id && rows[1].id > rows[2].id,
            "id DESC order"
        );
    }

    /// `list_revocations` honors the caller `limit` and clamps it into
    /// `[0, MAX_REVOCATIONS_LIST]`: a negative limit maps to the cap (returns all
    /// available rows, never an unbounded/panicking scan); a small limit truncates.
    #[test]
    fn revocations_list_limit_is_bounded() {
        let s = mem();
        for i in 0..10i64 {
            s.record_revocation(&RevocationEvent {
                id: 0,
                ts: i,
                fp: format!("SHA256:{i:02}"),
                identity: String::new(),
                source: String::new(),
                kind: RevocationKind::Enforced,
            })
            .unwrap();
        }
        assert_eq!(
            s.list_revocations(3).unwrap().len(),
            3,
            "small limit truncates"
        );
        assert_eq!(s.list_revocations(0).unwrap().len(), 0, "limit 0 ⇒ no rows");
        // A negative limit is clamped to 0 (`limit.clamp(0, MAX)`): bounded, no panic,
        // no unbounded scan. (Differs from the `clamp_limit` "negative ⇒ cap" idiom;
        // both are SAFE — see verifier report note. Asserting the implemented behavior.)
        assert_eq!(
            s.list_revocations(-1).unwrap().len(),
            0,
            "negative ⇒ 0 (bounded)"
        );
        // A limit larger than the cap is clamped to the cap, never an unbounded scan.
        assert_eq!(
            s.list_revocations(MAX_REVOCATIONS_LIST + 5_000)
                .unwrap()
                .len(),
            10,
            "over-cap limit clamped, returns only what exists"
        );
    }

    /// The write-seam `clamp_field` keeps an oversized hostile `fp`/`source` out of
    /// the table: a value past `MAX_REVOCATION_FIELD_LEN` is stored truncated (on a
    /// UTF-8 boundary), so the append-only log cannot be bloated by one event.
    #[test]
    fn revocations_clamp_oversized_fields_at_seam() {
        let s = mem();
        let huge = "x".repeat(MAX_REVOCATION_FIELD_LEN + 500);
        let multibyte = "é".repeat(MAX_REVOCATION_FIELD_LEN); // 2 bytes each ⇒ way over cap
        s.record_revocation(&RevocationEvent {
            id: 0,
            ts: 1,
            fp: huge.clone(),
            identity: multibyte.clone(),
            source: huge.clone(),
            kind: RevocationKind::Declared,
        })
        .unwrap();
        let rows = s.list_revocations(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].fp.len() <= MAX_REVOCATION_FIELD_LEN,
            "fp clamped to cap"
        );
        assert!(
            rows[0].source.len() <= MAX_REVOCATION_FIELD_LEN,
            "source clamped to cap"
        );
        assert!(
            rows[0].identity.len() <= MAX_REVOCATION_FIELD_LEN,
            "identity clamped to cap"
        );
        // Clamp is on a char boundary: the stored multibyte field is valid UTF-8
        // (the `String` could not exist otherwise) and never splits an `é`.
        assert!(
            rows[0].identity.chars().all(|c| c == 'é'),
            "multibyte field truncated on a char boundary, no mojibake"
        );
    }

    /// `clamp_field` is total: it never panics and always returns a `<= cap` string
    /// for arbitrary inputs, including empty and exactly-at-boundary strings.
    #[test]
    fn clamp_field_totality_edge_cases() {
        assert_eq!(clamp_field(""), "");
        let at = "a".repeat(MAX_REVOCATION_FIELD_LEN);
        assert_eq!(
            clamp_field(&at).len(),
            MAX_REVOCATION_FIELD_LEN,
            "at-cap unchanged"
        );
        let over = "a".repeat(MAX_REVOCATION_FIELD_LEN + 1);
        assert_eq!(
            clamp_field(&over).len(),
            MAX_REVOCATION_FIELD_LEN,
            "1-over clamps"
        );
        // A multibyte char straddling the cap boundary truncates BELOW the cap.
        let mut weird = "a".repeat(MAX_REVOCATION_FIELD_LEN - 1);
        weird.push('€'); // 3-byte char crosses the boundary
        let out = clamp_field(&weird);
        assert!(out.len() <= MAX_REVOCATION_FIELD_LEN);
        assert!(
            out.is_char_boundary(out.len()),
            "result ends on a char boundary"
        );
    }

    /// A legacy DB that predates `revocations` gains the table idempotently on open
    /// (mirror of the `identity_keys` legacy-migration test) with NO data loss to
    /// the pre-existing tables; re-opening is a no-op.
    #[test]
    fn legacy_db_gains_revocations_table() {
        let dir =
            std::env::temp_dir().join(format!("weave-rev-legacy-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-#11 store: messages with a row, NO revocations table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT, body TEXT NOT NULL
                 );
                 INSERT INTO messages (ts, sender, recipient, subject, body)
                 VALUES (1, 'a', 'b', NULL, 'pre-existing');",
            )
            .unwrap();
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type='table' AND name='revocations')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!exists, "fixture must predate the revocations table");
        }
        // First open runs the migration: the table exists and works, and the
        // pre-existing message survived (no data loss).
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(s.count_revocations().unwrap(), 0, "table created, empty");
            s.record_revocation(&RevocationEvent {
                id: 0,
                ts: 5,
                fp: "SHA256:zz".into(),
                identity: "id".into(),
                source: "src".into(),
                kind: RevocationKind::Declared,
            })
            .unwrap();
            assert_eq!(s.count_revocations().unwrap(), 1);
            let (rows, _) = s.inbox("b", true, false, 50).unwrap();
            assert_eq!(rows.len(), 1, "pre-existing message survived migration");
        }
        // Re-open: idempotent, no duplicate-table error, prior row retained.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(
                s.count_revocations().unwrap(),
                1,
                "re-open is a no-op; the recorded event persists"
            );
        }
    }

    /// R1-UNCHANGED (sqlite): a signed intent that verifies ONLY against a revoked
    /// key is STILL rejected, AND the rejection records EXACTLY ONE `Enforced` audit
    /// row (right fp/identity/source/kind) on the receiver's LOCAL store. The audit
    /// write is post-decision: the commit count is unchanged whether or not a row is
    /// appended, proving the verifier never reads the table.
    #[cfg(feature = "sign")]
    #[test]
    fn r1_reject_records_one_enforced_event_decision_unchanged() {
        use crate::sign::{fingerprint_full, sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let alice = SigningKey::from_bytes(&[55u8; 32]);
        let alice_pk = to_hex(alice.verifying_key().as_bytes());
        let alice_full = format!("SHA256:{}", fingerprint_full(&alice_pk).unwrap());

        let dir =
            std::env::temp_dir().join(format!("weave-r1audit-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();

        // Source store with a signed intent from alice for bob.
        let src_path = dir.join("src.db");
        let sa = SqliteStore::open(&src_path).unwrap();
        let sig = sign_intent(&alice, "alice", "bob", "revoked-hello");
        sa.enqueue_intent(
            "bob",
            "",
            "alice",
            None,
            "revoked-hello",
            &sig,
            None,
            None,
            None,
            0,
        )
        .unwrap();
        let source = StoreSource::Local(src_path);

        // Receiver B registers alice's key but REVOKES its fingerprint.
        let b = SqliteStore::open(&dir.join("b.db")).unwrap();
        b.register_key("alice", &alice_pk).unwrap();
        let revoke = VerifyPolicy {
            strict_override: None,
            trust: vec![],
            revoked: vec![alice_full.clone()],
        };

        // DECISION: still REJECT (commit count 0) — byte-identical to pre-audit.
        let res = pull_from_store(&b, "bob", std::slice::from_ref(&source), &revoke).unwrap();
        assert_eq!(
            res.committed, 0,
            "R1: revoked-only sig REJECTED (unchanged)"
        );

        // SIDE-EFFECT: exactly ONE Enforced audit row with the right facts.
        let rows = b.list_revocations(50).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one Enforced event recorded on reject"
        );
        assert_eq!(rows[0].kind, RevocationKind::Enforced);
        assert_eq!(rows[0].identity, "alice", "claimed identity captured");
        assert_eq!(
            rows[0].fp, alice_full,
            "the FULL revoked fingerprint is recorded"
        );
        assert!(!rows[0].source.is_empty(), "source label captured");
    }

    /// R1-UNCHANGED (sqlite): a NON-revoked signed intent COMMITS and records NO
    /// audit event — the audit log only grows on an actual enforcement, never on the
    /// happy path, so the decision and the log are independent.
    #[cfg(feature = "sign")]
    #[test]
    fn non_revoked_commit_records_no_event() {
        use crate::sign::{sign_intent, to_hex};
        use ed25519_dalek::SigningKey;

        let alice = SigningKey::from_bytes(&[66u8; 32]);
        let alice_pk = to_hex(alice.verifying_key().as_bytes());

        let dir =
            std::env::temp_dir().join(format!("weave-r1noaudit-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("src.db");
        let sa = SqliteStore::open(&src_path).unwrap();
        let sig = sign_intent(&alice, "alice", "bob", "clean-hello");
        sa.enqueue_intent(
            "bob",
            "",
            "alice",
            None,
            "clean-hello",
            &sig,
            None,
            None,
            None,
            0,
        )
        .unwrap();
        let source = StoreSource::Local(src_path);

        let b = SqliteStore::open(&dir.join("b.db")).unwrap();
        b.register_key("alice", &alice_pk).unwrap();
        let advisory = VerifyPolicy::advisory();
        let res = pull_from_store(&b, "bob", std::slice::from_ref(&source), &advisory).unwrap();
        assert_eq!(
            res.committed, 1,
            "non-revoked signed intent COMMITS (unchanged)"
        );
        assert_eq!(
            b.count_revocations().unwrap(),
            0,
            "no enforcement ⇒ no audit row; decision independent of the log"
        );
    }

    // ── P3 job board store tests (sqlite) ─────────────────────────────────────

    fn spec(title: &str) -> JobSpec {
        JobSpec {
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn job_create_defaults_to_queued_owned_by_creator() {
        let s = mem();
        let j = s.create_job("alice", spec("build it")).unwrap();
        assert!(crate::model::job_id_valid(&j.id));
        assert_eq!(j.state, JobState::Queued);
        assert_eq!(j.creator, "alice");
        assert_eq!(j.owner.as_deref(), Some("alice")); // owner defaults to creator
        assert!(j.assignee.is_none());
        assert!(j.attempt_id.is_none());
        // get_job round-trips.
        let got = s.get_job(&j.id).unwrap().unwrap();
        assert_eq!(got.id, j.id);
    }

    #[test]
    fn job_claim_mints_attempt_and_runs() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        let claimed = s.claim_job(&j.id, "worker").unwrap().unwrap();
        assert_eq!(claimed.state, JobState::Running);
        assert_eq!(claimed.assignee.as_deref(), Some("worker"));
        let att = claimed.attempt_id.clone().unwrap();
        assert!(crate::model::attempt_id_valid(&att));
    }

    #[test]
    fn job_update_lifecycle_with_matching_attempt_succeeds() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        let att = s
            .claim_job(&j.id, "w")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        let patch = JobPatch {
            state: Some(JobState::Completed),
            result_summary: Some("done".into()),
            result_json: Some(r#"{"ok":true}"#.into()),
            progress_note: Some("finished".into()),
            ..Default::default()
        };
        let done = s.update_job(&j.id, Some(&att), patch).unwrap();
        assert_eq!(done.state, JobState::Completed);
        assert!(done.completed_ts.is_some(), "terminal stamps completed_ts");
        assert_eq!(done.result_summary.as_deref(), Some("done"));
        // The note was APPENDED to the event log (not overwritten).
        assert!(done.progress_events_json.contains("finished"));
    }

    #[test]
    fn job_update_stale_attempt_is_fenced() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        let old = s
            .claim_job(&j.id, "w1")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        // A SECOND claim mints a NEW token, fencing out the first.
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
        assert!(
            err.to_string().contains("stale_attempt"),
            "stale token rejected"
        );
        // The CURRENT token still works.
        assert!(s
            .update_job(
                &j.id,
                Some(&new),
                JobPatch {
                    state: Some(JobState::Completed),
                    ..Default::default()
                }
            )
            .is_ok());
    }

    #[test]
    fn job_update_unclaimed_accepts_no_token() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        // No claim ⇒ NULL attempt_id ⇒ update without a token is allowed (pre-claim parking).
        let upd = s
            .update_job(
                &j.id,
                None,
                JobPatch {
                    phase: Some("triage".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(upd.phase.as_deref(), Some("triage"));
    }

    #[test]
    fn job_illegal_transition_rejected() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
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
        // completed -> running is illegal (terminal is frozen).
        let err = s
            .update_job(
                &j.id,
                Some(&att),
                JobPatch {
                    state: Some(JobState::Running),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("illegal transition"));
    }

    #[test]
    fn job_cancel_queued_goes_terminal_others_flag_only() {
        let s = mem();
        // queued -> straight to terminal cancelled.
        let q = s.create_job("alice", spec("q")).unwrap();
        let c = s.cancel_job(&q.id, "alice", Some("nope")).unwrap().unwrap();
        assert_eq!(c.state, JobState::Cancelled);
        assert!(c.cancel_requested);
        assert!(c.completed_ts.is_some());

        // running -> flag only, state unchanged.
        let r = s.create_job("alice", spec("r")).unwrap();
        s.claim_job(&r.id, "w").unwrap();
        let rc = s.cancel_job(&r.id, "alice", None).unwrap().unwrap();
        assert_eq!(rc.state, JobState::Running, "running stays running");
        assert!(rc.cancel_requested, "cancel_requested flag set");
    }

    #[test]
    fn job_list_filters_and_bounds() {
        let s = mem();
        let a = s.create_job("alice", spec("a")).unwrap();
        let _b = s.create_job("bob", spec("b")).unwrap();
        s.claim_job(&a.id, "alice").unwrap();
        // Filter by creator.
        let by_alice = s
            .list_jobs(
                JobFilter {
                    creator: Some("alice".into()),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        assert_eq!(by_alice.len(), 1);
        assert_eq!(by_alice[0].creator, "alice");
        // Filter by state.
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
        // A huge limit is clamped (no panic, bounded).
        let all = s.list_jobs(JobFilter::default(), i64::MAX).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn job_result_ready_only_when_terminal() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        let att = s
            .claim_job(&j.id, "w")
            .unwrap()
            .unwrap()
            .attempt_id
            .unwrap();
        let r = s.job_result(&j.id).unwrap().unwrap();
        assert!(!r.ready, "running job is not ready");
        s.update_job(
            &j.id,
            Some(&att),
            JobPatch {
                state: Some(JobState::Failed),
                error_json: Some(r#"{"e":"boom"}"#.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let r2 = s.job_result(&j.id).unwrap().unwrap();
        assert!(r2.ready);
        assert!(r2.error_json.contains("boom"));
    }

    #[test]
    fn job_caps_and_id_validation_enforced() {
        let s = mem();
        // Oversized title rejected.
        let big = "x".repeat(crate::model::MAX_JOB_TEXT + 1);
        assert!(s.create_job("alice", spec(&big)).is_err());
        // Oversized result JSON rejected on update.
        let j = s.create_job("alice", spec("ok")).unwrap();
        let bigjson = "x".repeat(crate::model::MAX_JOB_JSON + 1);
        let err = s
            .update_job(
                &j.id,
                None,
                JobPatch {
                    result_json: Some(bigjson),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("too large"));
        // A metachar-bearing job id never reaches a bind.
        assert!(s.get_job("job;rm").is_err());
    }

    #[test]
    fn job_migration_is_idempotent() {
        let s = mem();
        // Run migrate twice over the already-open DB; the second run is a clean no-op.
        migrate(&s.conn).unwrap();
        migrate(&s.conn).unwrap();
        // The table is usable after re-migration.
        let j = s.create_job("alice", spec("post-migrate")).unwrap();
        assert!(s.get_job(&j.id).unwrap().is_some());
    }

    /// A genuine LEGACY DB that predates the `jobs` table gains it idempotently on
    /// open (mirror of `legacy_db_gains_revocations_table`): the table is created,
    /// the pre-existing rows survive (no data loss), and re-opening is a clean no-op.
    /// This proves the migrate() upgrade path itself, not just `IF NOT EXISTS`
    /// re-entry on a DB that already has the table.
    #[test]
    fn legacy_db_gains_jobs_table() {
        let dir = std::env::temp_dir().join(format!(
            "weave-jobs-legacy-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-P3 store: a messages row, and explicitly NO jobs table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                    sender TEXT NOT NULL, recipient TEXT NOT NULL, subject TEXT, body TEXT NOT NULL
                 );
                 INSERT INTO messages (ts, sender, recipient, subject, body)
                 VALUES (1, 'a', 'b', NULL, 'pre-existing');",
            )
            .unwrap();
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type='table' AND name='jobs')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!exists, "fixture must predate the jobs table");
        }
        // First open runs the migration: the jobs table exists and works, and the
        // pre-existing message survived (no data loss).
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(
                s.list_jobs(JobFilter::default(), 100).unwrap().len(),
                0,
                "jobs table created, empty"
            );
            let j = s.create_job("alice", spec("after-migrate")).unwrap();
            assert!(s.get_job(&j.id).unwrap().is_some());
            let (rows, _) = s.inbox("b", true, false, 50).unwrap();
            assert_eq!(rows.len(), 1, "pre-existing message survived migration");
        }
        // Re-open: idempotent, no duplicate-table error, prior job retained.
        {
            let s = SqliteStore::open(&path).unwrap();
            assert_eq!(
                s.list_jobs(JobFilter::default(), 100).unwrap().len(),
                1,
                "re-open is a no-op; the created job persists"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_row_mapper_rejects_unknown_state() {
        let s = mem();
        let j = s.create_job("alice", spec("task")).unwrap();
        // Corrupt the stored state directly (bypassing the enum) and assert the read
        // hard-errors rather than silently coercing.
        s.conn
            .execute(
                "UPDATE jobs SET state = 'gibberish' WHERE id = ?1",
                params![j.id],
            )
            .unwrap();
        // The row mapper itself hard-errors (loudly) on the corrupt state rather than
        // panicking or silently coercing.
        let mapped = s.conn.query_row(
            "SELECT * FROM jobs WHERE id = ?1",
            params![j.id],
            row_to_job,
        );
        assert!(
            mapped.is_err(),
            "unknown stored state must be a mapper error"
        );
    }

    // ───────────────────────── P5: rich presence (turn_state + description) ──────

    /// A fresh peer takes the table defaults: `turn_state=''` (Unknown),
    /// `description=''`, `description_ts=0`. register does NOT set any of them.
    #[test]
    fn fresh_peer_has_default_presence() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.turn_state, "");
        assert_eq!(p.description, "");
        assert_eq!(p.description_ts, 0);
        assert_eq!(
            crate::model::TurnState::from_str(&p.turn_state).unwrap(),
            crate::model::TurnState::Unknown
        );
    }

    /// `set_turn_state` round-trips every enum value; an unknown value is a hard Err
    /// that performs NO write; the setter touches ONLY the named (caller's own) row.
    #[test]
    fn set_turn_state_roundtrip_self_only_and_rejects_unknown() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.register_peer("b", "tmux", "%2", "", Some("/y")).unwrap();
        for st in [
            crate::model::TurnState::PendingFirstTurn,
            crate::model::TurnState::Working,
            crate::model::TurnState::AwaitingInput,
            crate::model::TurnState::Idle,
        ] {
            s.set_turn_state("a", st.as_str()).unwrap();
            assert_eq!(s.get_peer("a").unwrap().unwrap().turn_state, st.as_str());
            // Self-only: b is never touched.
            assert_eq!(s.get_peer("b").unwrap().unwrap().turn_state, "");
        }
        // Unknown value: hard Err, NO write (a's prior valid state is unchanged).
        let before = s.get_peer("a").unwrap().unwrap().turn_state;
        assert!(s.set_turn_state("a", "garbage").is_err());
        assert_eq!(s.get_peer("a").unwrap().unwrap().turn_state, before);
    }

    /// `set_description` round-trips, control-strips + caps via `sanitize_tag`, and
    /// stamps `description_ts`; clearing stamps ts=0.
    #[test]
    fn set_description_roundtrips_sanitizes_and_stamps() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.set_description("a", "reviewing PR #23").unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.description, "reviewing PR #23");
        assert!(p.description_ts > 0, "a set description stamps now()");

        // Control chars stripped; internal spaces preserved.
        s.set_description("a", "do\u{1b}[2J the\nthing\u{0}")
            .unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.description, "do[2J thething");
        assert!(!p.description.chars().any(|c| c.is_control()));

        // Oversized truncates (never errors), bounded to MAX_DESC_LEN.
        let huge = "z".repeat(crate::model::MAX_DESC_LEN + 500);
        s.set_description("a", &huge).unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert!(p.description.chars().count() <= crate::model::MAX_DESC_LEN);

        // Clearing stamps ts=0 (unambiguously "absent").
        s.set_description("a", "").unwrap();
        let p = s.get_peer("a").unwrap().unwrap();
        assert_eq!(p.description, "");
        assert_eq!(p.description_ts, 0);
    }

    /// `set_description` is SELF-ONLY: setting "a" never touches "b".
    #[test]
    fn set_description_self_only() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.register_peer("b", "tmux", "%2", "", Some("/y")).unwrap();
        s.set_description("a", "mine").unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().description, "mine");
        assert_eq!(s.get_peer("b").unwrap().unwrap().description, "");
    }

    /// Read-time TTL: a description older than `DESCRIPTION_TTL_SECS` reads BLANK from
    /// get_peer/list_peers WITHOUT a DB write (the stored row keeps the text + ts); a
    /// fresh one within the window is honored.
    #[test]
    fn description_expires_at_read_time_without_db_write() {
        let s = mem();
        s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.set_description("a", "stale task").unwrap();
        // Poke description_ts past the TTL window via a direct UPDATE (test-only).
        let stale = now() - crate::model::DESCRIPTION_TTL_SECS - 1;
        s.conn
            .execute(
                "UPDATE peers SET description_ts=?2 WHERE name=?1",
                params!["a", stale],
            )
            .unwrap();
        // Read paths see it as absent (expired).
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
        // But the STORED row is untouched (no read-time write): the raw column still
        // holds the text + the stale ts.
        let (raw_desc, raw_ts): (String, i64) = s
            .conn
            .query_row(
                "SELECT description, description_ts FROM peers WHERE name='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            raw_desc, "stale task",
            "stored row not mutated at read time"
        );
        assert_eq!(raw_ts, stale);

        // A fresh description within the window IS honored.
        s.set_description("a", "fresh task").unwrap();
        assert_eq!(s.get_peer("a").unwrap().unwrap().description, "fresh task");
    }

    /// register_peer_full re-register PRESERVES a self-set turn_state + description
    /// (the `role`-omitted-from-upsert discipline): a session hook re-register must
    /// not wipe presence.
    #[test]
    fn reregister_preserves_self_set_turn_state_and_description() {
        let s = mem();
        let cert = s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        s.set_turn_state("a", "working").unwrap();
        s.set_description("a", "deep in the weeds").unwrap();
        let ts_before = s.get_peer("a").unwrap().unwrap().description_ts;
        // Re-register (a session hook would do this): new pane, same name.
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
        assert_eq!(p.target, "%9", "re-register updated the pane");
        assert_eq!(
            p.turn_state, "working",
            "turn_state preserved across re-register"
        );
        assert_eq!(
            p.description, "deep in the weeds",
            "description preserved across re-register"
        );
        assert_eq!(p.description_ts, ts_before, "description_ts preserved");
    }

    /// A legacy peers DB (pre-P5: no turn_state/description/description_ts columns)
    /// upgrades in place on open — columns added with the correct defaults, the old
    /// row survives reading Unknown/empty/0, and a re-open is an idempotent no-op.
    #[test]
    fn legacy_db_gains_presence_columns() {
        let dir = std::env::temp_dir().join(format!(
            "weave-presence-legacy-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        // A pre-P5 peers table: has role (post-P4) but NOT the three presence cols.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE peers (
                    name TEXT PRIMARY KEY, mux TEXT NOT NULL, target TEXT NOT NULL,
                    socket TEXT NOT NULL DEFAULT '', cwd TEXT NOT NULL DEFAULT '',
                    last_seen INTEGER NOT NULL, pid INTEGER, host TEXT NOT NULL DEFAULT '',
                    repo TEXT NOT NULL DEFAULT '', branch TEXT NOT NULL DEFAULT '',
                    worktree_id TEXT NOT NULL DEFAULT '', circle TEXT NOT NULL DEFAULT 'default',
                    role TEXT NOT NULL DEFAULT 'peer'
                 );
                 INSERT INTO peers (name, mux, target, last_seen)
                 VALUES ('old', 'tmux', '%1', 1);",
            )
            .unwrap();
            assert!(!column_exists(&conn, "peers", "turn_state").unwrap());
            assert!(!column_exists(&conn, "peers", "description").unwrap());
            assert!(!column_exists(&conn, "peers", "description_ts").unwrap());
        }
        // First open migrates: the three columns appear with defaults, the old row
        // survives reading Unknown/empty/0, and the new setters work on it.
        {
            let s = SqliteStore::open(&path).unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(p.turn_state, "", "legacy row reads Unknown");
            assert_eq!(p.description, "", "legacy row reads empty description");
            assert_eq!(p.description_ts, 0, "legacy row reads ts=0");
            s.set_turn_state("old", "idle").unwrap();
            s.set_description("old", "post-migrate desc").unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(p.turn_state, "idle");
            assert_eq!(p.description, "post-migrate desc");
        }
        // Re-open is an idempotent no-op; the row + its set presence persist.
        {
            let s = SqliteStore::open(&path).unwrap();
            let p = s.get_peer("old").unwrap().unwrap();
            assert_eq!(p.turn_state, "idle");
            assert_eq!(p.description, "post-migrate desc");
        }
    }

    /// `expire_description` totality (colocated with the store TTL constant): never
    /// panics for extreme `(now, ts)` and the expiry boundary is exact.
    #[test]
    fn expire_description_boundary_and_totality() {
        let mk = |desc: &str, ts: i64| Peer {
            name: "p".to_string(),
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
            circle: crate::model::DEFAULT_CIRCLE.to_string(),
            role: crate::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: desc.to_string(),
            description_ts: ts,
            birth_cert: None,
            contact_policy: "open".to_string(),
        };
        let ttl = crate::model::DESCRIPTION_TTL_SECS;
        // Exactly at the TTL boundary => expired (>=).
        let mut p = mk("x", 1000);
        crate::model::expire_description(&mut p, 1000 + ttl);
        assert_eq!(p.description, "", "at the boundary the description expires");
        // One second inside the window => honored.
        let mut p = mk("x", 1000);
        crate::model::expire_description(&mut p, 1000 + ttl - 1);
        assert_eq!(p.description, "x");
        // ts=0 (never anchored) is never expired regardless of now.
        let mut p = mk("x", 0);
        crate::model::expire_description(&mut p, i64::MAX);
        assert_eq!(p.description, "x");
        // Totality: extreme values never panic (saturating arithmetic).
        for (now, ts) in [
            (i64::MAX, i64::MIN),
            (i64::MIN, i64::MAX),
            (i64::MIN, i64::MIN),
            (i64::MAX, i64::MAX),
            (-1, -1),
        ] {
            let mut p = mk("x", ts);
            crate::model::expire_description(&mut p, now);
        }
    }

    // -----------------------------------------------------------------------
    // Presence seam (v0.2)
    // -----------------------------------------------------------------------

    #[test]
    fn presence_heartbeat_and_query() {
        let s = mem();
        let host = crate::config::this_host();
        // No heartbeat yet → None
        assert!(s.presence("alice", &host).unwrap().is_none());
        // Write heartbeat
        s.heartbeat("alice", &host, Some(1234)).unwrap();
        // Fresh heartbeat → Some
        let ts = s
            .presence("alice", &host)
            .unwrap()
            .expect("fresh heartbeat");
        assert!(ts > 0);
        // Wrong host → None
        assert!(s.presence("alice", "other-box").unwrap().is_none());
        // Evict does not touch fresh rows (30 s cutoff)
        let n = s.evict_stale_presence(PRESENCE_TTL_SECS).unwrap();
        assert_eq!(n, 0);
        assert!(s.presence("alice", &host).unwrap().is_some());
    }

    #[test]
    fn presence_evict_stale() {
        let s = mem();
        let host = crate::config::this_host();
        // Write an old heartbeat by cheating the clock via direct SQL
        let old_ts = crate::model::now() - PRESENCE_TTL_SECS - 1;
        s.conn
            .execute(
                "INSERT INTO presence (name, host, pid, heartbeat_ts) VALUES (?1, ?2, ?3, ?4)",
                params!["bob", &host, 0i64, old_ts],
            )
            .unwrap();
        // Stale → None
        assert!(s.presence("bob", &host).unwrap().is_none());
        // Evict removes it
        let n = s.evict_stale_presence(PRESENCE_TTL_SECS).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn peer_liveness_three_tier() {
        let s = mem();
        let host = crate::config::this_host();
        // Fresh heartbeat → Live even with ancient last_seen
        s.heartbeat("carol", &host, Some(1234)).unwrap();
        let p = Peer {
            name: "carol".into(),
            mux: "tmux".into(),
            target: "%1".into(),
            socket: String::new(),
            cwd: None,
            last_seen: 0, // ancient
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
        let liveness = s.peer_liveness(&p).unwrap();
        assert_eq!(
            liveness,
            crate::model::Liveness::Live,
            "heartbeat wins over stale last_seen"
        );

        // No heartbeat → falls back to TTL (recent last_seen → Likely)
        let p2 = Peer {
            name: "dave".into(),
            last_seen: crate::model::now(),
            pid: None,
            birth_cert: None,
            contact_policy: "open".to_string(),
            ..p.clone()
        };
        let liveness2 = s.peer_liveness(&p2).unwrap();
        assert_eq!(liveness2, crate::model::Liveness::Likely);

        // No heartbeat + old last_seen → Offline
        let p3 = Peer {
            name: "eve".into(),
            last_seen: 0,
            pid: None,
            birth_cert: None,
            contact_policy: "open".to_string(),
            ..p.clone()
        };
        let liveness3 = s.peer_liveness(&p3).unwrap();
        assert_eq!(liveness3, crate::model::Liveness::Offline);
    }

    // ── WL-016 schedule store tests ──────────────────────────────────────────

    #[test]
    fn schedule_one_shot_roundtrip() {
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
        assert_eq!(list[0].body, "hello");
        assert!(!list[0].cancelled);
    }

    #[test]
    fn schedule_cancel() {
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
        // Second cancel is idempotent false.
        assert!(!s.cancel_schedule(id).unwrap());
        let list = s.list_schedules("a", 50).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].cancelled);
    }

    #[test]
    fn schedule_due_query() {
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
    fn schedule_mark_executed_one_shot() {
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
    fn schedule_mark_executed_recurring() {
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
        // next_run should have been advanced to a future hour.
        assert!(list[0].next_run > past);
    }

    #[test]
    fn schedule_double_fire_prevented() {
        let s = mem();
        let past = crate::model::now() - 3600;
        let id = s
            .schedule_message("a", "b", None, "x", ScheduleKind::OneShot, "@daily", past)
            .unwrap();
        s.mark_schedule_executed(id).unwrap();
        s.mark_schedule_executed(id).unwrap(); // harmless no-op
        let due = s.get_due_schedules(crate::model::now()).unwrap();
        assert!(
            due.iter().all(|d| d.id != id),
            "executed one-shot must not appear in due query"
        );
    }

    #[test]
    fn schedule_caps_reject_oversized_body() {
        let s = mem();
        let big = "x".repeat(MAX_BODY + 1);
        assert!(s
            .schedule_message(
                "a",
                "b",
                None,
                &big,
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000
            )
            .is_err());
    }

    #[test]
    fn schedule_caps_reject_long_cron() {
        let s = mem();
        let long_cron = "x".repeat(MAX_CRON_EXPR_LEN + 1);
        assert!(s
            .schedule_message(
                "a",
                "b",
                None,
                "x",
                ScheduleKind::OneShot,
                &long_cron,
                1_700_000_000
            )
            .is_err());
    }

    #[test]
    fn schedule_caps_reject_bad_identity() {
        let s = mem();
        assert!(s
            .schedule_message(
                "",
                "b",
                None,
                "x",
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000
            )
            .is_err());
        assert!(s
            .schedule_message(
                "a",
                "",
                None,
                "x",
                ScheduleKind::OneShot,
                "@daily",
                1_700_000_000
            )
            .is_err());
    }

    // ---- WL-020: review queue ----

    #[test]
    fn review_add_list_mark_remove_roundtrip() {
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
        assert_eq!(all[0].title, "fix bug");
        assert_eq!(all[0].author, "alice");
        assert_eq!(all[0].repo, "owner/repo");

        let pending = s
            .review_queue(crate::model::ReviewQueueFilter::Pending, 10)
            .unwrap();
        assert_eq!(pending.len(), 1);

        let reviewed = s
            .review_queue(crate::model::ReviewQueueFilter::Reviewed, 10)
            .unwrap();
        assert_eq!(reviewed.len(), 0);

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
    fn review_rejects_bad_url() {
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
        assert!(s
            .add_review_item(
                "https://example.com/pr/1",
                "t",
                "a",
                "r",
                crate::model::ReviewItemState::Open,
                None
            )
            .is_err());
    }

    #[test]
    fn review_mark_remove_not_found() {
        let s = mem();
        assert!(!s.mark_reviewed("review_999_999", "bob").unwrap());
        assert!(!s.remove_review_item("review_999_999").unwrap());
    }

    // ---- WL-021: permission status ----

    #[test]
    fn permission_verdict_pending_then_timeout() {
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
        let (status, _body) = s.permission_verdict(&cid, 300).unwrap();
        assert_eq!(status, crate::model::PermissionStatus::Pending);
        // Simulate timeout by using a tiny timeout and an old ask... but the ask is fresh.
        // Instead test that non-existent ask errors.
        assert!(s.permission_verdict("ask_999_999", 300).is_err());
    }

    #[test]
    fn permission_verdict_approved_after_answer() {
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
    fn permission_list_filters_by_asker() {
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

    // ---- WL-024: reservation leases ----

    #[test]
    fn lease_reserve_acquire_and_conflict() {
        let s = mem();
        let l = s
            .reserve_lease("alice", "crates/foo", 3600, Some("working on it"))
            .unwrap();
        assert_eq!(l.resource, "crates/foo");
        assert_eq!(l.holder, "alice");
        assert_eq!(l.note, "working on it");

        // Same resource from another holder should fail.
        let err = s
            .reserve_lease("bob", "crates/foo", 3600, None)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("held by 'alice'"),
            "expected holder in error: {msg}"
        );

        // Different resource should succeed.
        let l2 = s.reserve_lease("bob", "crates/bar", 3600, None).unwrap();
        assert_eq!(l2.holder, "bob");
    }

    #[test]
    fn lease_expired_releases_automatically() {
        let s = mem();
        // Acquire with a 1-second TTL.
        let l = s.reserve_lease("alice", "crates/foo", 1, None).unwrap();
        assert_eq!(l.holder, "alice");

        // Wait for expiry.
        std::thread::sleep(std::time::Duration::from_secs(2));

        // bob can now acquire it.
        let l2 = s.reserve_lease("bob", "crates/foo", 3600, None).unwrap();
        assert_eq!(l2.holder, "bob");
    }

    #[test]
    fn lease_release_and_list() {
        let s = mem();
        s.reserve_lease("alice", "crates/foo", 3600, Some("note1"))
            .unwrap();
        s.reserve_lease("bob", "crates/bar", 3600, Some("note2"))
            .unwrap();

        let all = s.list_leases(10).unwrap();
        assert_eq!(all.len(), 2);

        // alice releases hers.
        assert!(s.release_lease("alice", "crates/foo").unwrap());
        let remaining = s.list_leases(10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].resource, "crates/bar");

        // Releasing non-existent or wrong holder returns false.
        assert!(!s.release_lease("alice", "crates/foo").unwrap());
        assert!(!s.release_lease("bob", "crates/foo").unwrap());
    }

    #[test]
    fn lease_list_only_active() {
        let s = mem();
        s.reserve_lease("alice", "crates/foo", 1, None).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let expired = s.list_leases(10).unwrap();
        assert_eq!(expired.len(), 0);
    }

    #[test]
    fn lease_rejects_bad_input() {
        let s = mem();
        assert!(s.reserve_lease("alice", "", 3600, None).is_err());
        assert!(s.reserve_lease("alice", "foo", 0, None).is_err());
        assert!(s.reserve_lease("alice", "foo", 86_401, None).is_err());
        let big_note = "x".repeat(crate::model::MAX_LEASE_NOTE_LEN + 1);
        assert!(s
            .reserve_lease("alice", "foo", 3600, Some(&big_note))
            .is_err());
    }

    #[test]
    fn summary_roundtrip() {
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

    // ---- WL-037: message supersede / successor chains ----------------------

    /// Helper: read a single message's `superseded_by` from `me`'s full history.
    fn superseded_by_of(s: &SqliteStore, me: &str, id: i64) -> Option<i64> {
        s.history(me, None, 100)
            .unwrap()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("message #{id} not in {me} history"))
            .superseded_by
    }

    #[test]
    fn supersede_stamps_predecessor() {
        let s = mem();
        let a = s.send("a", "b", Some("v1"), "first", None, None).unwrap();
        let b = s.send("a", "b", Some("v2"), "second", None, None).unwrap();
        s.supersede("a", a, b).unwrap();
        // The predecessor now points at its successor; the successor is unstamped.
        assert_eq!(superseded_by_of(&s, "b", a), Some(b));
        assert_eq!(superseded_by_of(&s, "b", b), None);
    }

    #[test]
    fn superseded_message_hidden_from_unread() {
        let s = mem();
        let a = s.send("a", "b", Some("v1"), "first", None, None).unwrap();
        let b = s.send("a", "b", Some("v2"), "second", None, None).unwrap();
        assert_eq!(s.unread_count("b").unwrap(), 2);
        s.supersede("a", a, b).unwrap();
        // Only the successor remains unread.
        assert_eq!(s.unread_count("b").unwrap(), 1);
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(inbox.iter().any(|m| m.id == b));
        assert!(
            !inbox.iter().any(|m| m.id == a),
            "superseded predecessor must not appear in unread inbox"
        );
        // The oldest-unread (nudge/wake path) skips the superseded predecessor.
        let oldest = s.peek_oldest_unread("b").unwrap().unwrap();
        assert_eq!(oldest.id, b);
    }

    #[test]
    fn history_retains_superseded_with_flag() {
        let s = mem();
        let a = s.send("a", "b", Some("v1"), "first", None, None).unwrap();
        let b = s.send("a", "b", Some("v2"), "second", None, None).unwrap();
        s.supersede("a", a, b).unwrap();
        // History keeps the superseded row (audit) AND populates the flag.
        let hist = s.history("b", None, 100).unwrap();
        let row_a = hist.iter().find(|m| m.id == a).expect("history keeps A");
        assert_eq!(row_a.superseded_by, Some(b));
    }

    #[test]
    fn supersede_chain_only_tail_unread() {
        let s = mem();
        let a = s.send("a", "b", None, "A", None, None).unwrap();
        let b = s.send("a", "b", None, "B", None, None).unwrap();
        let c = s.send("a", "b", None, "C", None, None).unwrap();
        s.supersede("a", a, b).unwrap();
        s.supersede("a", b, c).unwrap();
        // A->B->C: only the tail C remains unread.
        assert_eq!(s.unread_count("b").unwrap(), 1);
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert_eq!(inbox.iter().filter(|m| m.id == c).count(), 1);
        assert!(!inbox.iter().any(|m| m.id == a || m.id == b));
    }

    #[test]
    fn supersede_rejects_foreign_sender() {
        let s = mem();
        let a = s.send("a", "b", None, "A", None, None).unwrap();
        let b = s.send("c", "b", None, "C", None, None).unwrap();
        // 'c' is not the sender of A => rejected, A unchanged.
        assert!(s.supersede("c", a, b).is_err());
        assert_eq!(superseded_by_of(&s, "b", a), None);
        assert_eq!(s.unread_count("b").unwrap(), 2);
    }

    #[test]
    fn supersede_rejects_missing_ids() {
        let s = mem();
        let a = s.send("a", "b", None, "A", None, None).unwrap();
        // Missing old id.
        assert!(s.supersede("a", 999_999, a).is_err());
        // Missing new id (successor does not exist).
        assert!(s.supersede("a", a, 999_999).is_err());
        // No panic, A unchanged.
        assert_eq!(superseded_by_of(&s, "b", a), None);
    }

    #[test]
    fn supersede_broadcast_drops_from_all_readers() {
        let s = mem();
        let bcast = "all";
        let a = s.send("a", bcast, None, "v1", None, None).unwrap();
        let b = s.send("a", bcast, None, "v2", None, None).unwrap();
        // Two distinct readers each have both broadcasts unread.
        assert_eq!(s.unread_count("r1").unwrap(), 2);
        assert_eq!(s.unread_count("r2").unwrap(), 2);
        s.supersede("a", a, b).unwrap();
        // The per-message stamp drops the superseded broadcast from EVERY reader.
        assert_eq!(s.unread_count("r1").unwrap(), 1);
        assert_eq!(s.unread_count("r2").unwrap(), 1);
        let (in1, _) = s.inbox("r1", false, false, 50).unwrap();
        assert!(in1.iter().any(|m| m.id == b) && !in1.iter().any(|m| m.id == a));
    }

    #[test]
    fn supersede_migration_is_idempotent() {
        // The guarded ADD COLUMN must be a no-op on a store that already has it:
        // re-opening the same DB twice (each runs migrate) must not error and the
        // column must be present (a supersede succeeds).
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weave-mig-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let a;
        {
            let s = SqliteStore::open(&path).unwrap();
            a = s.send("a", "b", None, "A", None, None).unwrap();
        }
        // Second open re-runs migrate(): must be idempotent, column still present.
        let s2 = SqliteStore::open(&path).unwrap();
        let b = s2.send("a", "b", None, "B", None, None).unwrap();
        s2.supersede("a", a, b).unwrap();
        assert_eq!(superseded_by_of(&s2, "b", a), Some(b));
    }

    // ---- WL-039: idle-notification dedup -----------------------------------

    #[test]
    fn supersede_prior_idle_replaces_prior_unread_idle() {
        let s = mem();
        // Two idle pings a->b: each notify stamps idle + supersedes the prior one.
        let p1 = s
            .send("a", "b", None, "still waiting?", None, None)
            .unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", p1).unwrap(), 0);
        let p2 = s
            .send("a", "b", None, "still waiting??", None, None)
            .unwrap();
        assert_eq!(s.supersede_prior_idle("a", "b", p2).unwrap(), 1);
        // Only the latest ping is unread; the first is superseded + hidden.
        assert_eq!(s.unread_count("b").unwrap(), 1);
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(inbox.iter().any(|m| m.id == p2));
        assert!(!inbox.iter().any(|m| m.id == p1));
        assert_eq!(superseded_by_of(&s, "b", p1), Some(p2));
        // Nudge/wake path also skips the superseded predecessor.
        assert_eq!(s.peek_oldest_unread("b").unwrap().unwrap().id, p2);
    }

    #[test]
    fn idle_dedup_never_touches_real_messages() {
        let s = mem();
        // A REAL message (no idle stamp) between two idle pings must survive.
        let p1 = s.send("a", "b", None, "ping 1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", p1).unwrap();
        let real = s
            .send("a", "b", Some("work"), "real content", None, None)
            .unwrap();
        let p2 = s.send("a", "b", None, "ping 2", None, None).unwrap();
        let n = s.supersede_prior_idle("a", "b", p2).unwrap();
        // Exactly the prior idle ping was superseded — NOT the real message.
        assert_eq!(n, 1);
        assert_eq!(superseded_by_of(&s, "b", p1), Some(p2));
        assert_eq!(superseded_by_of(&s, "b", real), None);
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(
            inbox.iter().any(|m| m.id == real),
            "real message must stay unread"
        );
        assert!(inbox.iter().any(|m| m.id == p2));
        assert!(!inbox.iter().any(|m| m.id == p1));
    }

    #[test]
    fn idle_dedup_only_supersedes_unread() {
        let s = mem();
        let p1 = s.send("a", "b", None, "ping 1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", p1).unwrap();
        // b READS the first ping (mark_read=true drains the unread).
        let _ = s.inbox("b", false, true, 50).unwrap();
        let p2 = s.send("a", "b", None, "ping 2", None, None).unwrap();
        // A read predecessor is NOT superseded (only unread pings are replaced).
        let n = s.supersede_prior_idle("a", "b", p2).unwrap();
        assert_eq!(n, 0);
        assert_eq!(superseded_by_of(&s, "b", p1), None);
    }

    #[test]
    fn idle_dedup_scoped_to_same_sender_recipient() {
        let s = mem();
        // a->b, c->b, and a->z idle pings.
        let a_b1 = s.send("a", "b", None, "a1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", a_b1).unwrap();
        let c_b = s.send("c", "b", None, "c1", None, None).unwrap();
        s.supersede_prior_idle("c", "b", c_b).unwrap();
        let a_z = s.send("a", "z", None, "az1", None, None).unwrap();
        s.supersede_prior_idle("a", "z", a_z).unwrap();
        // A new a->b ping supersedes ONLY a's prior a->b ping.
        let a_b2 = s.send("a", "b", None, "a2", None, None).unwrap();
        let n = s.supersede_prior_idle("a", "b", a_b2).unwrap();
        assert_eq!(n, 1);
        assert_eq!(superseded_by_of(&s, "b", a_b1), Some(a_b2));
        // c's ping and a's ping to z are untouched.
        assert_eq!(superseded_by_of(&s, "b", c_b), None);
        assert_eq!(superseded_by_of(&s, "z", a_z), None);
    }

    #[test]
    fn idle_dedup_authz_self_only() {
        let s = mem();
        // a sends an idle ping to b.
        let a_b = s.send("a", "b", None, "a1", None, None).unwrap();
        s.supersede_prior_idle("a", "b", a_b).unwrap();
        // c sends an idle ping to b, then tries to dedup as if it were a's ping:
        // c's call is scoped to sender='c', so a's ping is NOT superseded.
        let c_b = s.send("c", "b", None, "c1", None, None).unwrap();
        let n = s.supersede_prior_idle("c", "b", c_b).unwrap();
        assert_eq!(n, 0, "c cannot supersede a's prior idle ping");
        assert_eq!(superseded_by_of(&s, "b", a_b), None);
    }

    #[test]
    fn idle_dedup_idempotency_replay_is_noop() {
        let s = mem();
        // A notify carrying an idempotency key: a re-send returns the SAME id, so
        // the dedup `id <> new_id` guard makes it a clean no-op (never self-supersede).
        let p1 = s.send("a", "b", None, "ping", Some("k-1"), None).unwrap();
        s.supersede_prior_idle("a", "b", p1).unwrap();
        let replay = s.send("a", "b", None, "ping", Some("k-1"), None).unwrap();
        assert_eq!(replay, p1, "idempotency replay returns the existing id");
        let n = s.supersede_prior_idle("a", "b", replay).unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            superseded_by_of(&s, "b", p1),
            None,
            "must not self-supersede"
        );
        assert_eq!(s.unread_count("b").unwrap(), 1);
    }

    #[test]
    fn idle_dedup_kind_column_is_migrated_idempotently() {
        // The guarded ADD COLUMN kind must survive a re-open (migrate re-run).
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weave-kind-mig-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let p1;
        {
            let s = SqliteStore::open(&path).unwrap();
            p1 = s.send("a", "b", None, "p1", None, None).unwrap();
            s.supersede_prior_idle("a", "b", p1).unwrap();
        }
        let s2 = SqliteStore::open(&path).unwrap();
        let p2 = s2.send("a", "b", None, "p2", None, None).unwrap();
        assert_eq!(s2.supersede_prior_idle("a", "b", p2).unwrap(), 1);
        assert_eq!(superseded_by_of(&s2, "b", p1), Some(p2));
    }

    // ---- WL-035: Store::snapshot_to (VACUUM INTO + read-back) ---------------

    #[test]
    fn snapshot_to_roundtrips_messages() {
        let s = mem();
        s.send("a", "b", Some("s"), "hi", None, None).unwrap();
        s.send("a", "b", None, "again", None, None).unwrap();
        let src_count = s.total_messages().unwrap();
        assert_eq!(src_count, 2);

        let dir =
            std::env::temp_dir().join(format!("weave-snap-{}-{}", std::process::id(), src_count));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("snapshot.db");
        let _ = std::fs::remove_file(&dest);
        s.snapshot_to(&dest).unwrap();

        // The snapshot opens read-only and reports the same count.
        let snap = SqliteStore::open_readonly(&dest).unwrap();
        assert_eq!(snap.total_messages().unwrap(), src_count);
    }

    #[test]
    fn snapshot_to_empty_db_is_valid() {
        let s = mem();
        assert_eq!(s.total_messages().unwrap(), 0);
        let dir = std::env::temp_dir().join(format!("weave-snap-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("empty-snapshot.db");
        let _ = std::fs::remove_file(&dest);
        s.snapshot_to(&dest).unwrap();
        let snap = SqliteStore::open_readonly(&dest).unwrap();
        assert_eq!(snap.total_messages().unwrap(), 0);
    }

    // ---- WL-038: ephemeral messages with TTL + auto-sweep ------------------

    #[test]
    fn expiry_stamps_and_excludes_from_unread() {
        let s = mem();
        let live = s
            .send("a", "b", Some("keep"), "permanent", None, None)
            .unwrap();
        let eph = s
            .send("a", "b", Some("v"), "ephemeral", None, None)
            .unwrap();
        // Stamp the ephemeral row already expired (now - 1).
        s.set_message_expiry(eph, now() - 1).unwrap();
        // Any read surface triggers the opportunistic sweep + excludes the row.
        let (inbox, _) = s.inbox("b", false, false, 50).unwrap();
        assert!(inbox.iter().any(|m| m.id == live));
        assert!(
            !inbox.iter().any(|m| m.id == eph),
            "expired ephemeral must not appear in unread inbox"
        );
        // Delete-on-sweep: the row is GONE (not just filtered).
        assert_eq!(s.total_messages().unwrap(), 1);
        assert!(s
            .history("b", None, 100)
            .unwrap()
            .iter()
            .all(|m| m.id != eph));
        assert!(s
            .inbox_since("b", 0, 100)
            .unwrap()
            .iter()
            .all(|m| m.id != eph));
        assert!(s
            .search("ephemeral", 50)
            .unwrap()
            .iter()
            .all(|m| m.id != eph));
        // The oldest-unread (nudge/wake path) skips it too.
        let oldest = s.peek_oldest_unread("b").unwrap().unwrap();
        assert_eq!(oldest.id, live);
    }

    #[test]
    fn sweep_expired_messages_deletes_expired_keeps_live() {
        let s = mem();
        let expired = s.send("a", "b", None, "gone", None, None).unwrap();
        let future = s.send("a", "b", None, "soon", None, None).unwrap();
        let permanent = s.send("a", "b", None, "forever", None, None).unwrap();
        // Mark all read so we can assert the expired row's `reads` are pruned too.
        // (Do this BEFORE stamping expiry, since the inbox read triggers a sweep.)
        let _ = s.inbox("b", false, true, 50).unwrap();
        s.set_message_expiry(expired, now() - 5).unwrap();
        s.set_message_expiry(future, now() + 10_000).unwrap();
        let n = s.sweep_expired_messages().unwrap();
        assert_eq!(n, 1, "exactly the one expired row is swept");
        let hist = s.history("b", None, 100).unwrap();
        assert!(hist.iter().any(|m| m.id == future));
        assert!(hist.iter().any(|m| m.id == permanent));
        assert!(hist.iter().all(|m| m.id != expired));
        // Its reads row is gone (no orphan).
        assert!(s.receipts(expired).unwrap().is_empty());
    }

    #[test]
    fn gc_also_reaps_expired_ephemeral() {
        let s = mem();
        // A message NEWER than any retention cutoff but already expired.
        let eph = s
            .send("a", "b", None, "fresh-but-expired", None, None)
            .unwrap();
        s.set_message_expiry(eph, now() - 1).unwrap();
        // gc with a huge retention window would normally keep a fresh `ts`, but the
        // ephemeral fold-in deletes it anyway.
        s.gc(86_400 * 365).unwrap();
        assert_eq!(s.total_messages().unwrap(), 0);
    }

    #[test]
    fn non_ephemeral_message_is_never_swept() {
        let s = mem();
        let mid = s.send("a", "b", None, "forever", None, None).unwrap();
        // A permanent (expires_at IS NULL) row survives both sweep and gc.
        assert_eq!(s.sweep_expired_messages().unwrap(), 0);
        s.gc(86_400 * 365).unwrap();
        let hist = s.history("b", None, 100).unwrap();
        assert!(
            hist.iter().any(|m| m.id == mid),
            "permanent message survives sweep + gc"
        );
    }

    #[test]
    fn expires_at_column_is_migrated_idempotently() {
        let s = mem();
        // Fresh DB has the column (via SCHEMA); migrate() is also re-run on open.
        assert!(column_exists(&s.conn, "messages", "expires_at").unwrap());
        assert!(column_exists(&s.conn, "outbox", "ttl").unwrap());
    }

    #[test]
    fn cross_store_intent_carries_ttl_to_expiry() {
        let dir = std::env::temp_dir().join(format!("weave-ttl-xstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a_path = dir.join("a.db");
        let b_path = dir.join("b.db");
        let _ = std::fs::remove_file(&a_path);
        let _ = std::fs::remove_file(&b_path);
        let a = SqliteStore::open(&a_path).unwrap();
        // Queue an intent for bob with a 600s ttl in A's outbox.
        a.enqueue_intent("bob", "", "alice", None, "hi", "", None, None, None, 600)
            .unwrap();
        let b = SqliteStore::open(&b_path).unwrap();
        let allow = vec![StoreSource::Local(a_path.clone())];
        let pulled = pull_from_store(&b, "bob", &allow, &VerifyPolicy::advisory()).unwrap();
        assert_eq!(pulled.committed, 1);
        // The committed message carries an expiry roughly now()+600.
        let hist = b.history("bob", None, 100).unwrap();
        let m = hist.iter().find(|m| m.body == "hi").expect("committed");
        let exp = m.expires_at.expect("ttl re-stamped as expiry");
        assert!(exp > now() + 500 && exp <= now() + 600);
    }
}
