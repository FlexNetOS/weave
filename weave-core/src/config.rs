//! Configuration: optional `~/.config/weave/config.toml`, overlaid by env vars.
//!
//! ```toml
//! session        = "desktop"      # default identity for this machine/session
//! backend        = "sqlite"       # "sqlite" (default) | "libsql"
//! db             = "/path/to/messages.db"
//! nudge_template = "[weave] msg from {from}: {body} — run weave_inbox"
//! libsql_url        = "libsql://..."   # only for backend = "libsql"
//! libsql_auth_token = "..."
//! ```

use serde::Deserialize;
use std::path::PathBuf;

/// A resolved federation / Tier-2 pull source: either a LOCAL store file path or a
/// REMOTE libSQL/Turso endpoint URL (with an optional auth token). Backend-agnostic
/// data (no I/O); produced by [`Config::peer_db_sources`] / [`pull_from_sources`]
/// and consumed by the store-layer free functions. Lives in `config` (below
/// `store`/`mcp`/`main`) so no upward dependency is introduced.
///
/// A remote source is opened READ-ONLY (and only on a `--features libsql` build);
/// the default sqlite build rejects it loudly at the store seam. weave NEVER writes
/// a remote/foreign store — see the store-layer owner-only-writes guards.
#[derive(Clone, PartialEq, Eq)]
pub enum StoreSource {
    /// A local store file path (existing behavior).
    Local(PathBuf),
    /// A remote libSQL/Turso endpoint. `token` is a SECRET (redacted in Debug,
    /// never logged/injected/argv'd). `timeout_ms` is the resolved, clamped
    /// per-source remote-call timeout (NOT a secret): `Some(ms)` ⇒ the store bounds
    /// every connect/SELECT on this source by that value; `None` ⇒ the store falls
    /// back to its global/default bound (identical to pre-per-source behavior). The
    /// value is resolved in `config` (see [`per_source_timeout`]) so the store needs
    /// no per-source context.
    Remote {
        url: String,
        token: Option<String>,
        timeout_ms: Option<u64>,
    },
}

// Manual Debug that REDACTS the remote auth token (mirrors the `Config` Debug
// redaction) so a `{:?}` can never leak the secret via a log line, panic message,
// or error context. The URL is shown (it is not itself a secret), but the token is
// only ever rendered as `<redacted>`. `timeout_ms` is a plain integer (not a
// secret) and is shown verbatim.
impl std::fmt::Debug for StoreSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreSource::Local(p) => f.debug_tuple("Local").field(p).finish(),
            StoreSource::Remote {
                url,
                token,
                timeout_ms,
            } => f
                .debug_struct("Remote")
                .field("url", url)
                .field("token", &token.as_ref().map(|_| "<redacted>"))
                .field("timeout_ms", timeout_ms)
                .finish(),
        }
    }
}

impl StoreSource {
    /// True for a remote (URL) source.
    pub fn is_remote(&self) -> bool {
        matches!(self, StoreSource::Remote { .. })
    }
}

/// URL schemes recognized as a REMOTE libSQL/Turso source (the schemes
/// `libsql::Builder::new_remote` accepts). Anything not starting with one of these
/// is treated as a local file path (the existing bare-path / `./x.db` / `/abs/x.db`
/// behavior).
const REMOTE_SCHEMES: &[&str] = &["libsql://", "https://", "http://", "wss://", "ws://"];

/// Classify a single (already-trimmed) source entry as [`StoreSource::Remote`] iff
/// it begins with a recognized remote scheme, else [`StoreSource::Local`]. Pure; no
/// I/O and no canonicalization (a URL must never be `std::fs::canonicalize`'d). The
/// token is NOT attached here — the resolver layers the shared `pull_token` on.
pub fn classify_source(entry: &str) -> StoreSource {
    if REMOTE_SCHEMES.iter().any(|s| entry.starts_with(s)) {
        StoreSource::Remote {
            url: entry.to_string(),
            token: None,
            timeout_ms: None,
        }
    } else {
        StoreSource::Local(PathBuf::from(entry))
    }
}

/// Upper bound (bytes) on a remote auth token. A Turso JWT is well under this; the
/// cap bounds a hostile/garbage env value before it is handed to the libsql client
/// (it never reaches a shell or SQL — bound as a client arg — but bounding +
/// control-char-rejecting it keeps the value sane). Mirrors the [`MAX_HOST_LEN`]
/// discipline.
pub const MAX_TOKEN_LEN: usize = 8192;

/// Normalize a remote URL for dedup / cursor-key stability: trim a single trailing
/// slash so `libsql://h/` and `libsql://h` map to one source. NOT canonicalization.
fn normalize_remote_url(url: &str) -> String {
    url.strip_suffix('/').unwrap_or(url).to_string()
}

/// Validate + sanitize a remote auth token before use: reject control characters
/// (an injection/garbage canary) and bound it to [`MAX_TOKEN_LEN`] bytes. Returns
/// the token unchanged on success. The token is treated as a secret throughout
/// (never logged); a rejected token yields `None` (the source is attempted without
/// a token, which a server-enforced read-only deployment may still allow, or fails
/// closed at connect — never a panic).
fn sanitize_token(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    if token.len() > MAX_TOKEN_LEN {
        eprintln!("[weave] ignoring pull_token: too long (max {MAX_TOKEN_LEN} bytes)");
        return None;
    }
    if token.chars().any(|c| c.is_control()) {
        eprintln!("[weave] ignoring pull_token: contains control characters");
        return None;
    }
    Some(token.to_string())
}

/// Upper bound (chars) on an inline source LABEL (the `LABEL=` prefix that selects
/// a per-source `WEAVE_PULL_TOKEN_<LABEL>` env var). A label is NOT a secret — it
/// only names which env var holds the token — but it is bounded + charset-restricted
/// so it canonicalizes to a legal env-var suffix and can never carry an unbounded or
/// hostile value. Generous for any human-chosen label.
pub const MAX_LABEL_LEN: usize = 64;

/// Which token tier resolved for a remote source, for token-FREE `doctor`
/// observability. NEVER carries the token bytes or the label↔token pairing; it is a
/// pure classification of WHERE the source's token came from (or that none applied).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PullTokenTier {
    /// A per-source `WEAVE_PULL_TOKEN_<LABEL>` was set, sane, and applied.
    PerSourceLabel,
    /// The shared `pull_token` / `WEAVE_PULL_TOKEN` applied (no valid per-source).
    Shared,
    /// No token applied (no per-source label-env and no shared token).
    None,
}

/// Is `s` a valid inline source label: non-empty, ≤ [`MAX_LABEL_LEN`] chars, and
/// every char in `[A-Za-z0-9_]` (so it canonicalizes to a legal env-var suffix
/// after uppercasing). Pure; total on any input. A label is the LEFT side of an
/// inline `LABEL=<remote-url>` entry; it is not a secret.
fn is_valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_LABEL_LEN
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a single (already-trimmed) source entry into an optional inline LABEL and
/// the [`StoreSource`] it governs. A WRAPPER around [`classify_source`] that adds
/// label recognition WITHOUT changing `classify_source`'s behavior or totality.
///
/// The entry is parsed as `LABEL=<rest>` iff ALL of:
///   1. it contains an `=`, AND
///   2. the substring before the first `=` is a [valid label](is_valid_label), AND
///   3. the substring after the first `=` classifies as a [`StoreSource::Remote`].
///
/// On a match the label is UPPERCASED (so `prod=` and `PROD=` select the same env
/// var) and returned alongside the `Remote` source built from `<rest>`. If ANY
/// condition fails, the WHOLE original entry is passed verbatim to `classify_source`
/// with no label — so bare paths/URLs, a local path containing `=` (its right side
/// is not a remote URL), and a malformed label all degrade exactly as today. A label
/// is ONLY ever recognized on a remote URL (there is no per-source token for a local
/// file). Pure; total on any input.
fn parse_labeled_source(entry: &str) -> (Option<String>, StoreSource) {
    if let Some((label, rest)) = entry.split_once('=') {
        if is_valid_label(label) {
            let inner = classify_source(rest);
            if inner.is_remote() {
                return (Some(label.to_ascii_uppercase()), inner);
            }
        }
    }
    (None, classify_source(entry))
}

/// Resolve a remote source's auth token by precedence, returning the token (a
/// SECRET) AND the token-free [`PullTokenTier`] for diagnostics:
///   1. per-source `WEAVE_PULL_TOKEN_<LABEL>` (exact `env::var` lookup — NO
///      `env::vars()` scan — when the entry carried a valid label AND the var is set
///      AND [`sanitize_token`] accepts it) ⇒ [`PullTokenTier::PerSourceLabel`];
///   2. else the shared (already-sanitized) token ⇒ [`PullTokenTier::Shared`];
///   3. else `None` ⇒ [`PullTokenTier::None`].
///
/// `label` is already validated/uppercased. A per-source env var that is set but
/// REJECTED by `sanitize_token` (over-cap / control chars) FALLS THROUGH to the
/// shared token (it does not suppress it) — the loud stderr note already fired. The
/// label is used ONLY to build the exact env-var name; it never travels with the
/// token and is never logged alongside it.
fn per_source_token(label: Option<&str>, shared: Option<&str>) -> (Option<String>, PullTokenTier) {
    if let Some(l) = label {
        if let Some(v) = nonempty(&format!("WEAVE_PULL_TOKEN_{l}")) {
            if let Some(t) = sanitize_token(&v) {
                return (Some(t), PullTokenTier::PerSourceLabel);
            }
        }
    }
    match shared {
        Some(s) => (Some(s.to_string()), PullTokenTier::Shared),
        None => (None, PullTokenTier::None),
    }
}

/// Default wall-clock bound (ms) for a single REMOTE network call (connect or a
/// SELECT). SINGLE SOURCE OF TRUTH: the store-layer fallback imports this const, so
/// the config-resolved path and the store-fallback path can never disagree on the
/// default (drift guard — mirrors the `BROADCAST_SQL` byte-identity discipline). A
/// remote-call timeout is NEVER disabled (an unbounded remote could hang a drain).
pub const REMOTE_TIMEOUT_MS_DEFAULT: u64 = 5_000;

/// Lower clamp on a resolved remote-call timeout: below this a remote can essentially
/// never succeed, so a foot-gun `WEAVE_PULL_TIMEOUT_MS=1` is raised to a value that
/// can plausibly connect rather than turning every remote into an instant skip.
pub const MIN_TIMEOUT_MS: u64 = 50;

/// Upper clamp on a resolved remote-call timeout (10 minutes): a generous hard ceiling
/// so a hostile/garbage huge value (`WEAVE_PULL_TIMEOUT_MS=99999999999`) cannot make a
/// drain hang ~forever. Mirrors the `clamp_limit` input-cap discipline.
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Lower clamp on the `weave sessions --watch` poll interval (seconds). A `0`s
/// interval would busy-spin the read loop, so a foot-gun `--interval 0` is raised
/// to this floor. Mirrors the `MIN_TIMEOUT_MS` input-cap discipline.
pub const WATCH_INTERVAL_MIN_SECS: u64 = 1;

/// Upper clamp on the `weave sessions --watch` poll interval (1 hour): a hostile /
/// garbage huge value (`--interval 99999999999`) cannot freeze the dashboard for an
/// absurd span. Mirrors the `MAX_TIMEOUT_MS` ceiling discipline.
pub const WATCH_INTERVAL_MAX_SECS: u64 = 3_600;

/// Clamp an untrusted `weave sessions --watch` interval (seconds) into
/// `[WATCH_INTERVAL_MIN_SECS, WATCH_INTERVAL_MAX_SECS]`. Pure; total on any `u64`
/// (a `0` clamps UP to the floor, an enormous value clamps DOWN to the ceiling) and
/// idempotent. Mirrors the `parse_clamp_timeout` clamp idiom but takes an already-
/// parsed `u64` (the clap `--interval` is `u64`), so it never disables the bound.
pub fn clamp_watch_interval(secs: u64) -> u64 {
    secs.clamp(WATCH_INTERVAL_MIN_SECS, WATCH_INTERVAL_MAX_SECS)
}

/// Which timeout tier resolved for a remote source, for token-FREE `doctor`
/// observability. A pure classification of WHERE the source's effective remote-call
/// timeout came from. Mirrors [`PullTokenTier`]; carries no secret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PullTimeoutTier {
    /// A per-source `WEAVE_PULL_TIMEOUT_MS_<LABEL>` was set, sane, and applied.
    PerSourceLabel,
    /// The global `WEAVE_PULL_TIMEOUT_MS` applied (no valid per-source value).
    Global,
    /// Neither was set/valid ⇒ the hardcoded [`REMOTE_TIMEOUT_MS_DEFAULT`] applies.
    Default,
}

/// Token-FREE per-source diagnostics bundle for a resolved REMOTE source: which token
/// tier and which timeout tier resolved. Carries NO secret (no token bytes, no
/// label↔token pairing) — only the two classifications, for `doctor` aggregation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RemoteTiers {
    pub token: PullTokenTier,
    pub timeout: PullTimeoutTier,
}

/// Token-FREE per-source-kind federation rollup for `doctor` observability. Carries
/// ONLY counts and a plain effective-ms range for ONE source kind (`peer_db` OR
/// `pull_from`) — NEVER a token byte, NEVER a label↔token pairing. Used to render the
/// secret-free "federation health" block in both `weave doctor` (CLI) and the
/// `weave_doctor` MCP tool. `ms_min`/`ms_max` are `None` when the kind has no remote
/// source (so an empty set never renders a misleading `0-0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FederationKindHealth {
    /// Total resolved sources for this kind (local + remote).
    pub total: usize,
    /// Local-file sources.
    pub local: usize,
    /// Remote-URL sources (the set the token/timeout tiers are counted over).
    pub remote: usize,
    /// Remote sources whose token resolved per-source (`WEAVE_PULL_TOKEN_<LABEL>`).
    pub token_per_source: usize,
    /// Remote sources that fell back to the shared `pull_token`.
    pub token_shared: usize,
    /// Remote sources with no token applied.
    pub token_none: usize,
    /// Remote sources whose timeout resolved per-source (`WEAVE_PULL_TIMEOUT_MS_<LABEL>`).
    pub timeout_per_source: usize,
    /// Remote sources that fell back to the global `WEAVE_PULL_TIMEOUT_MS`.
    pub timeout_global: usize,
    /// Remote sources using the hardcoded [`REMOTE_TIMEOUT_MS_DEFAULT`].
    pub timeout_default: usize,
    /// Minimum effective remote-call timeout (ms) over the remote sources; `None`
    /// when there is no remote source for this kind.
    pub ms_min: Option<u64>,
    /// Maximum effective remote-call timeout (ms) over the remote sources; `None`
    /// when there is no remote source for this kind.
    pub ms_max: Option<u64>,
}

/// Secret-free federation-health rollup for `doctor`, aggregating BOTH source kinds
/// symmetrically: `peer_db` (Tier-1 federation) and `pull_from` (Tier-2 delivery).
/// Carries ONLY counts + an effective-ms range per kind — NEVER a token. Built by
/// [`Config::federation_health`] so the CLI `weave doctor` and the `weave_doctor`
/// MCP tool consume ONE method and cannot drift. Reachability for the `peer_db` set
/// is NOT recomputed here (no new probe): `doctor` derives ok/skipped from the
/// already-computed [`crate::store::federation_status`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FederationHealth {
    /// Rollup over the `peer_db` (Tier-1 federation) source set.
    pub peer_db: FederationKindHealth,
    /// Rollup over the `pull_from` (Tier-2 delivery) source set.
    pub pull_from: FederationKindHealth,
}

/// Parse + clamp a remote-call timeout env value: parse as `u64`, require `> 0`, and
/// clamp into `[MIN_TIMEOUT_MS, MAX_TIMEOUT_MS]`. Returns `None` on an empty /
/// unparsable / `0` value so the caller FALLS THROUGH to the next precedence tier
/// (never disabling the bound). Pure; total on any input. A `0`/garbage value never
/// disables the timeout — an unbounded remote could hang a drain.
fn parse_clamp_timeout(s: &str) -> Option<u64> {
    let n: u64 = s.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(n.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS))
}

/// Resolve a remote source's effective remote-call timeout (ms) by precedence,
/// returning the resolved value AND the token-free [`PullTimeoutTier`] for
/// diagnostics. Mirrors [`per_source_token`] exactly:
///   1. per-source `WEAVE_PULL_TIMEOUT_MS_<LABEL>` (exact `env::var` lookup, when the
///      entry carried a valid label AND the var parses+clamps via
///      [`parse_clamp_timeout`]) ⇒ [`PullTimeoutTier::PerSourceLabel`];
///   2. else the global `WEAVE_PULL_TIMEOUT_MS` (same parse+clamp) ⇒
///      [`PullTimeoutTier::Global`];
///   3. else `None` ⇒ [`PullTimeoutTier::Default`] (the store applies
///      [`REMOTE_TIMEOUT_MS_DEFAULT`]).
///
/// `label` is already validated/uppercased. A per-source value that is set but
/// unparsable / `0` / out-of-range FALLS THROUGH to the global (then default),
/// mirroring the token `sanitize_token` fall-through. The returned `Option<u64>` is
/// `Some(clamped_ms)` for tiers 1–2 and `None` for the default tier (the store
/// supplies the default), so a `StoreSource::Remote` carrying `None` behaves exactly
/// as today. Pure (env-reading); total on any label.
fn per_source_timeout(label: Option<&str>) -> (Option<u64>, PullTimeoutTier) {
    if let Some(l) = label {
        if let Some(v) = nonempty(&format!("WEAVE_PULL_TIMEOUT_MS_{l}")) {
            if let Some(ms) = parse_clamp_timeout(&v) {
                return (Some(ms), PullTimeoutTier::PerSourceLabel);
            }
        }
    }
    if let Some(v) = nonempty("WEAVE_PULL_TIMEOUT_MS") {
        if let Some(ms) = parse_clamp_timeout(&v) {
            return (Some(ms), PullTimeoutTier::Global);
        }
    }
    (None, PullTimeoutTier::Default)
}

/// Platform path-list separator accepted in `WEAVE_PEER_DBS` (in addition to the
/// canonical comma): `;` on Windows, `:` elsewhere — matching `PATH` semantics.
/// NOT the path *component* separator (`/`), which must never split a list entry.
#[cfg(windows)]
const PEER_DBS_LIST_SEP: char = ';';
#[cfg(not(windows))]
const PEER_DBS_LIST_SEP: char = ':';

/// Split a `WEAVE_PEER_DBS`/`WEAVE_PULL_FROM`/`WEAVE_ALLOW_INJECT_FROM` env value
/// into individual entries. The canonical separator is the COMMA; the platform
/// path-list separator ([`PEER_DBS_LIST_SEP`]) is also accepted for convenience.
///
/// CRITICAL (Tier-2 v2): a REMOTE URL entry (`libsql://h`, `https://h`, …) contains
/// the unix path-list separator `:` inside `scheme://`, so a naive `split(':')` would
/// shred a URL into `libsql` + `//h`. We therefore split on the COMMA first (which a
/// URL never contains as a separator here), and only then apply the platform `:`/`;`
/// split to fragments that are NOT recognized remote URLs. A remote URL fragment is
/// passed through whole. Blank fragments are dropped; trimming/NUL-reject/cap happen
/// later in [`resolve_store_sources`].
///
/// Tier-2 v2 follow-up (per-source tokens): an inline `LABEL=<remote-url>` fragment
/// ALSO embeds the `:` inside its `scheme://` (after the `LABEL=` prefix), so it must
/// be treated as opaque too — otherwise `MYDB=libsql://h/db` would be shredded into
/// `MYDB=libsql` + `//h/db` and the label feature would silently never engage via the
/// env path. We recognize it with the SAME canonical label rule the resolver uses:
/// a valid label left of the first `=` and a remote-scheme URL on the right.
fn split_source_list(v: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for comma_part in v.split(',') {
        let part = comma_part.trim();
        if part.is_empty() {
            continue;
        }
        // A recognized remote URL is opaque: never split it on the path separator.
        if is_opaque_remote_fragment(part) {
            out.push(part.to_string());
            continue;
        }
        for seg in part.split(PEER_DBS_LIST_SEP) {
            let seg = seg.trim();
            if !seg.is_empty() {
                out.push(seg.to_string());
            }
        }
    }
    out
}

/// Split a `WEAVE_TRUST`/`WEAVE_REVOKED` env value into individual fingerprint/
/// pubkey entries. Unlike [`split_source_list`], this splits ONLY on the comma (and
/// whitespace/newlines) — NEVER on the platform `:` separator, because a fingerprint
/// is literally `SHA256:<hex>` and splitting it on `:` would shred every entry. Blank
/// fragments are dropped; trimming/cap/dedup happen later in [`resolve_fp_list`].
fn split_fp_list(v: &str) -> Vec<String> {
    v.split([',', '\n', '\r', '\t', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a validated, deduplicated TRUST/REVOKED list (free fn; no `&self` needed):
/// trims blanks, rejects any entry containing a NUL/control char or longer than
/// [`MAX_FP_ENTRY_LEN`], dedups preserving first-seen order, and caps the count at
/// [`MAX_TRUST`] with a one-line stderr note. `list_label` names the list for that
/// note. Default (`None`) ⇒ `[]`. Pure policy data — never a path, never opened.
fn resolve_fp_list(raw: Option<&[String]>, list_label: &str) -> Vec<String> {
    let raw = match raw {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_FP_ENTRY_LEN {
            eprintln!("[weave] skipping over-long {list_label} entry (> {MAX_FP_ENTRY_LEN} chars)");
            continue;
        }
        if trimmed.chars().any(|c| c == '\0' || c.is_control()) {
            eprintln!("[weave] skipping invalid {list_label} entry (control character)");
            continue;
        }
        let e = trimmed.to_string();
        if out.contains(&e) {
            continue;
        }
        out.push(e);
    }
    if out.len() > MAX_TRUST {
        eprintln!(
            "[weave] {} {list_label} entries configured; capping at {MAX_TRUST}",
            out.len()
        );
        out.truncate(MAX_TRUST);
    }
    out
}

/// A list fragment that must NEVER be split on the platform path separator because it
/// embeds `:` inside a `scheme://`: either a bare remote URL (`libsql://h`) or an
/// inline `LABEL=<remote-url>` (`MYDB=libsql://h`). Mirrors the canonical label rule
/// in [`parse_labeled_source`] so the env split and the resolver agree on what a
/// labelled remote is (no drift).
fn is_opaque_remote_fragment(part: &str) -> bool {
    if REMOTE_SCHEMES.iter().any(|s| part.starts_with(s)) {
        return true;
    }
    if let Some((label, rest)) = part.split_once('=') {
        return is_valid_label(label) && REMOTE_SCHEMES.iter().any(|s| rest.starts_with(s));
    }
    false
}

/// Hard ceiling on how many extra read-only stores Tier-1 federation will open
/// in one `weave peers`/`sessions` call. Each extra store is an open + N+1 list
/// fan-out, so an unbounded (or hostile) `WEAVE_PEER_DBS` could turn one listing
/// into thousands of file opens. Entries beyond this cap are dropped with a
/// stderr note. Generous for any real multi-project mesh.
pub const MAX_PEER_DBS: usize = 16;

/// Hard ceiling on how many `pull_from` source stores Tier-2 cross-store delivery
/// will pull intents from in one drain. Each source is a read-only open + an
/// `outbox` SELECT + per-intent local commit, so an unbounded (or hostile)
/// `WEAVE_PULL_FROM` could turn one drain into thousands of file opens. Entries
/// beyond this cap are dropped with a stderr note. Mirrors [`MAX_PEER_DBS`].
pub const MAX_PULL_FROM: usize = 16;

/// Hard ceiling on how many entries a signed-identity TRUST or REVOKED list may
/// hold (`WEAVE_TRUST` / `WEAVE_REVOKED` / `config.trust` / `config.revoked`).
/// Each entry is a bounded fingerprint/pubkey string consulted only on the `sign`
/// pull path; an unbounded list could bloat per-intent verification. Entries beyond
/// this cap are dropped with a stderr note. Mirrors [`MAX_PULL_FROM`].
pub const MAX_TRUST: usize = 64;

/// Per-entry character cap for a TRUST/REVOKED list entry. Generous: a full
/// `SHA256:<64-hex>` fingerprint is 71 chars and a bare 32-byte pubkey hex is 64;
/// this bound (mirroring `sign::MAX_KEY_HEX_LEN`) rejects an unbounded/hostile entry
/// at the config seam without coupling `config` to the feature-gated `sign` module.
pub const MAX_FP_ENTRY_LEN: usize = 256;

/// Default message-retention window: 30 days in seconds. A `session` hook GC pass
/// (see `Config::retention`) deletes messages older than this. Mirrors the
/// `weave gc` CLI default so the opportunistic and explicit sweeps agree.
pub const DEFAULT_RETENTION_SECS: i64 = 2_592_000;

#[derive(Deserialize, Default, Clone)]
pub struct Config {
    pub session: Option<String>,
    pub backend: Option<String>,
    pub db: Option<String>,
    pub nudge_template: Option<String>,
    pub libsql_url: Option<String>,
    pub libsql_auth_token: Option<String>,
    /// Age threshold (seconds) for the opportunistic GC run at SessionStart.
    /// `None` ⇒ the [`DEFAULT_RETENTION_SECS`] default (30 days). A value of `0`
    /// disables the auto-GC entirely (messages are kept until an explicit
    /// `weave gc`). Negative values are treated as `0` (disabled).
    pub retention_secs: Option<i64>,
    /// Additional, **read-only** store files to aggregate peers/sessions from
    /// (Tier-1 federation). Each entry is a path to another weave SQLite/libsql
    /// local DB file; weave opens them read-only to *see* their peers/sessions
    /// without ever writing them. `None`/empty ⇒ single-store behavior identical
    /// to today. Overlaid by `WEAVE_PEER_DBS` (comma- or path-separator list).
    /// `#[serde(default)]` keeps configs that omit the key loading unchanged.
    #[serde(default)]
    pub peer_dbs: Option<Vec<String>>,
    /// Tier-2 cross-store **delivery** sources: store files this session will pull
    /// directed intents FROM and **commit into its own inbox** (via the normal
    /// local `Store::send`). This is a STRICTLY HIGHER trust grant than
    /// [`peer_dbs`](Self::peer_dbs): `peer_dbs` only lets weave *see* other stores'
    /// peers/sessions (read-only, cannot mutate my inbox), whereas `pull_from`
    /// lets an allow-listed source deliver a message into my inbox. The two are
    /// kept DISTINCT so adding a store merely to view it never silently upgrades it
    /// into a delivery source (a privilege-escalation footgun). A path may appear
    /// in both lists. `None`/empty ⇒ no Tier-2 delivery (identical to today).
    /// Overlaid by `WEAVE_PULL_FROM` (comma- or path-separator list).
    /// `#[serde(default)]` keeps configs that omit the key loading unchanged.
    #[serde(default)]
    pub pull_from: Option<Vec<String>>,
    /// Tier-2 consent (DEFAULT ON): when a pull commits a message from an
    /// allow-listed source, also fire the existing paste-safe **content-free**
    /// nudge into THIS session's OWN registered pane (never a foreign pane). The
    /// body already landed in the inbox on commit; the nudge is only a "check your
    /// inbox" ping. `None` ⇒ **enabled** ([`inject_pulled`](Self::inject_pulled)).
    /// Set `false` for pure queue-only delivery (the single off-switch — the
    /// message still delivers, just without the live nudge). Overlaid by
    /// `WEAVE_INJECT_PULLED` (a truthy/falsy value).
    #[serde(default)]
    pub inject_pulled: Option<bool>,
    /// Optional finer gate on which pull sources may trigger the consent nudge.
    /// When set, ONLY a source whose path is in this list injects (others still
    /// deliver to the inbox, just silently — never a keystroke). When unset, EVERY
    /// [`pull_from`](Self::pull_from) source is inject-eligible, since being on the
    /// pull list is already the higher trust grant: the recommended relationship is
    /// "same as the pull set" (anyone you already accept delivery from can also
    /// nudge you), with this list available to NARROW that to a subset. `None` ⇒
    /// "same as pull_from". Overlaid by `WEAVE_ALLOW_INJECT_FROM` (comma- or
    /// path-separator list). Capped at [`MAX_PULL_FROM`].
    #[serde(default)]
    pub allow_inject_from: Option<Vec<String>>,
    /// Tier-2 signed identity (2d, only meaningful in a `--features sign` build):
    /// when `true`, a pulled cross-store intent that is UNSIGNED or cannot be
    /// cryptographically attributed to its claimed `from` is DROPPED rather than
    /// committed under the advisory allowlist model. `None`/`false` ⇒ the advisory
    /// fallback (commit unsigned intents, exactly as 2a–2c). A tampered/forged
    /// signature is ALWAYS rejected regardless of this flag. Inert without the
    /// `sign` feature. Overlaid by `WEAVE_STRICT_VERIFY` (a truthy/falsy value).
    #[serde(default)]
    pub strict_verify: Option<bool>,
    /// Tier-2 signed identity (2d, only meaningful in a `--features sign` build):
    /// the receiver's TRUST SET — a list of trusted sender fingerprints
    /// (`SHA256:<full-64-hex>`) or bare full pubkey hex strings. When NON-EMPTY a
    /// trust set is "configured": a sender whose registered key's fingerprint is in
    /// this list is verified STRICTLY (a bad/missing signature from them is REJECTED,
    /// not warned). Senders OUTSIDE the trust set keep the advisory model (unsigned
    /// operation unchanged). Empty/`None` ⇒ no trust set ⇒ behavior identical to
    /// today. Each entry compares against the FULL SHA-256 digest (the truncated
    /// display form is never trusted). Overlaid by `WEAVE_TRUST` (comma- or
    /// path-separator list). Inert without the `sign` feature.
    #[serde(default)]
    pub trust: Option<Vec<String>>,
    /// Tier-2 signed identity (2d, only meaningful in a `--features sign` build):
    /// the REVOCATION LIST — fingerprints (`SHA256:<full-64-hex>`) or bare full
    /// pubkey hex strings whose signatures are NO LONGER accepted. A signature that
    /// verifies against a revoked key's fingerprint is REJECTED unconditionally
    /// (even when `strict_verify = Some(false)`): the global-disable toggle governs
    /// only the unsigned/unknown advisory path, never a revoked key's signed message
    /// (R1). Empty/`None` ⇒ nothing revoked. Overlaid by `WEAVE_REVOKED` (comma- or
    /// path-separator list). Inert without the `sign` feature.
    #[serde(default)]
    pub revoked: Option<Vec<String>>,
    /// Tier-2 v2 shared auth token applied to every REMOTE (`libsql://`/`https://`/
    /// `wss://`) `pull_from`/`peer_dbs` source that does not carry its own. Treat as
    /// a SECRET: it is redacted in [`Config`]'s Debug, never logged/injected/argv'd,
    /// and bounded ([`MAX_TOKEN_LEN`]) + control-char-rejected before use. Prefer the
    /// `WEAVE_PULL_TOKEN` env var over storing it in the config file. `None` ⇒ remote
    /// sources are attempted without a weave-supplied token (a server-enforced
    /// read-only Turso token is the recommended deployment contract — see docs).
    /// Inert on the default sqlite build (remote sources are rejected loudly there).
    #[serde(default)]
    pub pull_token: Option<String>,
    /// Visibility-scoping circle this session belongs to (P4): a grouping label
    /// (NOT a secret, NOT a path). `peers`/`sessions`/`scan` default to the
    /// caller's circle; `--all-circles`/`circle='*'` go mesh-wide. `None`/empty ⇒
    /// the [`crate::model::DEFAULT_CIRCLE`] (`"default"`), so a single-circle
    /// deployment is identical to today. Overlaid by `WEAVE_CIRCLE`. Resolved (and
    /// validated) by [`Config::circle`]. `#[serde(default)]` keeps configs that
    /// omit the key loading unchanged.
    #[serde(default)]
    pub circle: Option<String>,
}

// Manual Debug that REDACTS the libSQL auth token so it can never leak via a
// `{:?}` in a log line, panic message, or error context.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("session", &self.session)
            .field("backend", &self.backend)
            .field("db", &self.db)
            .field("nudge_template", &self.nudge_template)
            .field("libsql_url", &self.libsql_url)
            .field(
                "libsql_auth_token",
                &self.libsql_auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("retention_secs", &self.retention_secs)
            .field("peer_dbs", &self.peer_dbs)
            .field("pull_from", &self.pull_from)
            .field("inject_pulled", &self.inject_pulled)
            .field("allow_inject_from", &self.allow_inject_from)
            .field("strict_verify", &self.strict_verify)
            .field("trust", &self.trust)
            .field("revoked", &self.revoked)
            .field(
                "pull_token",
                &self.pull_token.as_ref().map(|_| "<redacted>"),
            )
            .field("circle", &self.circle)
            .finish()
    }
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The XDG-default store path (`$XDG_DATA_HOME/weave/messages.db`, else
/// `~/.local/share/weave/messages.db`), ignoring any config/env `db` override.
/// Exposed so diagnostics (`weave doctor` / `weave_doctor`) can warn when the
/// *resolved* `db_path()` points somewhere other than this well-known store — the
/// most common "why can't I see the other session's peers" cause is each session
/// pointing at a different `WEAVE_DB`.
pub fn default_db_path() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"))
        .join("weave")
        .join("messages.db")
}

pub fn config_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
        .join("weave")
        .join("config.toml")
}

/// Upper bound (chars) on a derived host identifier. A host label is persisted on
/// every peer row and is only used to gate "is this PID mine to probe", so it
/// needs to be stable per machine, not long. We cap + control-char-sanitize it the
/// same way identities are bounded so a hostile `$HOSTNAME` / `/etc/hostname` can
/// never inject an unbounded or control-bearing value into the store.
pub const MAX_HOST_LEN: usize = 128;

/// A stable per-machine host identifier, used (with a peer's PID) to gate real
/// process-liveness: a PID is only probed when `peer.host == this_host()`.
///
/// Resolution order: `$HOSTNAME` → first line of `/etc/hostname` → `"local"`.
/// The result is trimmed, has any control characters stripped, and is truncated
/// to [`MAX_HOST_LEN`] chars on a UTF-8 boundary; if sanitizing empties it, we
/// fall back to `"local"`. This keeps the value bounded and control-free, treating
/// it like an identity cap (it never reaches a shell or SQL literal — it is bound
/// as a parameter — but bounding it keeps the store and any display safe).
pub fn this_host() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let raw = raw.or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .and_then(|s| s.lines().next().map(|l| l.to_string()))
            .filter(|s| !s.trim().is_empty())
    });
    let host = raw.unwrap_or_else(|| "local".to_string());
    let cleaned: String = host
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_HOST_LEN)
        .collect();
    if cleaned.is_empty() {
        "local".to_string()
    } else {
        cleaned
    }
}

impl Config {
    /// Load from disk (if present) and overlay environment overrides.
    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

        if let Some(v) = nonempty("WEAVE_SESSION") {
            cfg.session = Some(v);
        }
        if let Some(v) = nonempty("WEAVE_BACKEND") {
            cfg.backend = Some(v);
        }
        if let Some(v) = nonempty("WEAVE_DB") {
            cfg.db = Some(v);
        }
        if let Some(v) = nonempty("WEAVE_LIBSQL_URL") {
            cfg.libsql_url = Some(v);
        }
        if let Some(v) = nonempty("WEAVE_LIBSQL_AUTH_TOKEN") {
            cfg.libsql_auth_token = Some(v);
        }
        // Retention override: a non-numeric value is ignored (leaves the config /
        // default in place) rather than silently disabling GC.
        if let Some(v) = nonempty("WEAVE_RETENTION_SECS").and_then(|s| s.parse::<i64>().ok()) {
            cfg.retention_secs = Some(v);
        }
        // Federation peer stores: WEAVE_PEER_DBS is a list of extra read-only DB
        // file paths. The canonical separator is the COMMA (documented), and we
        // also accept the platform path-list separator (`:` on unix, `;` on
        // windows) for convenience. We deliberately do NOT split on the path
        // COMPONENT separator (`/`), which would shred any absolute path. The env
        // list is UNIONED onto any config `peer_dbs` (env appended), matching the
        // env-augments-config posture elsewhere. Validation/cap/dedup happens in
        // `peer_db_paths`.
        if let Some(v) = nonempty("WEAVE_PEER_DBS") {
            let env_paths = split_source_list(&v);
            if !env_paths.is_empty() {
                let mut merged = cfg.peer_dbs.take().unwrap_or_default();
                merged.extend(env_paths);
                cfg.peer_dbs = Some(merged);
            }
        }
        // Tier-2 delivery sources: WEAVE_PULL_FROM is a list of store paths this
        // session will pull directed intents from. Same split rules and
        // env-unions-config posture as WEAVE_PEER_DBS above. Validation/cap/dedup
        // happens in `pull_from_paths`.
        if let Some(v) = nonempty("WEAVE_PULL_FROM") {
            let env_paths = split_source_list(&v);
            if !env_paths.is_empty() {
                let mut merged = cfg.pull_from.take().unwrap_or_default();
                merged.extend(env_paths);
                cfg.pull_from = Some(merged);
            }
        }
        // Tier-2 consent toggle: WEAVE_INJECT_PULLED overrides the config/default.
        // Accepts the usual truthy/falsy spellings; an unrecognized value is
        // IGNORED (leaves the config/default in place) rather than silently
        // flipping the switch. Default (unset config + unset env) ⇒ ON.
        if let Some(v) = nonempty("WEAVE_INJECT_PULLED").and_then(|s| parse_bool(&s)) {
            cfg.inject_pulled = Some(v);
        }
        // Tier-2 finer inject gate: WEAVE_ALLOW_INJECT_FROM is a list of source
        // paths permitted to trigger the consent nudge. Same split rules and
        // env-unions-config posture as WEAVE_PULL_FROM. Validation/cap/dedup
        // happens in `allow_inject_from_paths`.
        if let Some(v) = nonempty("WEAVE_ALLOW_INJECT_FROM") {
            let env_paths = split_source_list(&v);
            if !env_paths.is_empty() {
                let mut merged = cfg.allow_inject_from.take().unwrap_or_default();
                merged.extend(env_paths);
                cfg.allow_inject_from = Some(merged);
            }
        }
        // Tier-2 signed-identity strictness: WEAVE_STRICT_VERIFY overrides the
        // config/default. Accepts the usual truthy/falsy spellings; an unrecognized
        // value is IGNORED (leaves the config/default in place). Default ⇒ OFF
        // (advisory fallback). Inert without the `sign` feature.
        if let Some(v) = nonempty("WEAVE_STRICT_VERIFY").and_then(|s| parse_bool(&s)) {
            cfg.strict_verify = Some(v);
        }
        // Tier-2 signed-identity TRUST SET: WEAVE_TRUST is a list of trusted sender
        // fingerprints (or full pubkey hex). Same split rules and env-unions-config
        // posture as WEAVE_PULL_FROM above. Validation/cap/dedup happens in
        // `trust_set`. Inert without the `sign` feature.
        if let Some(v) = nonempty("WEAVE_TRUST") {
            let env_entries = split_fp_list(&v);
            if !env_entries.is_empty() {
                let mut merged = cfg.trust.take().unwrap_or_default();
                merged.extend(env_entries);
                cfg.trust = Some(merged);
            }
        }
        // Tier-2 signed-identity REVOCATION LIST: WEAVE_REVOKED is a list of revoked
        // fingerprints (or full pubkey hex). Same discipline as WEAVE_TRUST.
        if let Some(v) = nonempty("WEAVE_REVOKED") {
            let env_entries = split_fp_list(&v);
            if !env_entries.is_empty() {
                let mut merged = cfg.revoked.take().unwrap_or_default();
                merged.extend(env_entries);
                cfg.revoked = Some(merged);
            }
        }
        // Tier-2 v2 shared remote auth token: WEAVE_PULL_TOKEN overrides the config
        // `pull_token`. A secret — never logged here (the value is not echoed). The
        // env var is the PREFERRED way to supply it (kept out of the config file).
        if let Some(v) = nonempty("WEAVE_PULL_TOKEN") {
            cfg.pull_token = Some(v);
        }
        // P4 circle: WEAVE_CIRCLE overrides the config `circle`. Stored raw here;
        // validated/sanitized at the `Config::circle()` resolve seam (the
        // session/host discipline — overlay then validate, never store-side trust).
        if let Some(v) = nonempty("WEAVE_CIRCLE") {
            cfg.circle = Some(v);
        }
        cfg
    }

    /// Resolve this session's visibility-scoping circle (P4). The config/env value
    /// if it passes [`crate::model::circle_valid`]; otherwise the
    /// [`crate::model::DEFAULT_CIRCLE`] (with a one-line stderr note for an invalid
    /// value, the `sanitize_token`/`this_host` discipline — sanitize at the seam,
    /// never store/inject a raw untrusted token). `None`/empty ⇒ `"default"`, so a
    /// single-circle deployment is byte-identical to today.
    pub fn circle(&self) -> String {
        match self.circle.as_deref().filter(|s| !s.is_empty()) {
            Some(c) if crate::model::circle_valid(c) => c.to_string(),
            Some(c) => {
                eprintln!(
                    "[weave] ignoring invalid circle {c:?}; falling back to '{}'",
                    crate::model::DEFAULT_CIRCLE
                );
                crate::model::DEFAULT_CIRCLE.to_string()
            }
            None => crate::model::DEFAULT_CIRCLE.to_string(),
        }
    }

    /// Resolve the validated, deduplicated list of **read-only** extra store paths
    /// to aggregate (Tier-1 federation). Pure path resolution (no I/O beyond the
    /// already-loaded config/env):
    ///
    /// - drops blank entries and any path containing a NUL byte (a classic
    ///   injection canary that cannot be a real file path);
    /// - **drops any path equal to the local [`db_path`](Self::db_path)** — after
    ///   canonicalizing both where possible — so the local store is never opened a
    ///   second time / double-counted;
    /// - deduplicates (canonical-aware) while preserving first-seen order;
    /// - caps the count at [`MAX_PEER_DBS`], printing a one-line stderr note when
    ///   truncating so the drop is diagnosable (never stdout).
    ///
    /// Default (no env, no config key) ⇒ `[]` ⇒ behavior identical to today.
    ///
    /// Transitional Local-only wrapper kept alongside the new
    /// [`peer_db_sources`](Self::peer_db_sources) for bisectability (Q-R3); all
    /// production callers use `*_sources`, so it is exercised only by config unit
    /// tests — `allow(dead_code)` marks that intentional retention, not dead code.
    #[allow(dead_code)]
    pub fn peer_db_paths(&self) -> Vec<PathBuf> {
        self.resolve_store_list(self.peer_dbs.as_deref(), MAX_PEER_DBS, "WEAVE_PEER_DBS")
    }

    /// Resolve the validated, deduplicated list of Tier-2 **delivery** source
    /// stores (`pull_from`). Identical validation discipline to [`peer_db_paths`]
    /// (trim, reject NUL, drop the local `db_path`, canonical-dedup, cap at
    /// [`MAX_PULL_FROM`] with a stderr note), but keyed off the DISTINCT
    /// `pull_from` list — being a read-only *visibility* source (`peer_dbs`) does
    /// not make a store a *delivery* source. Default (no env, no config key) ⇒
    /// `[]` ⇒ no cross-store delivery (identical-to-today).
    ///
    /// Transitional Local-only wrapper kept alongside
    /// [`pull_from_sources`](Self::pull_from_sources) for bisectability (Q-R3); see
    /// [`peer_db_paths`](Self::peer_db_paths).
    #[allow(dead_code)]
    pub fn pull_from_paths(&self) -> Vec<PathBuf> {
        self.resolve_store_list(self.pull_from.as_deref(), MAX_PULL_FROM, "WEAVE_PULL_FROM")
    }

    /// Tier-2 v2: resolve the Tier-1 federation sources as [`StoreSource`]s (local
    /// paths AND remote URLs). Supersedes [`peer_db_paths`](Self::peer_db_paths) (now
    /// a Local-only wrapper kept transitionally). Remote entries carry the shared
    /// `pull_token`. See [`resolve_store_sources`](Self::resolve_store_sources).
    pub fn peer_db_sources(&self) -> Vec<StoreSource> {
        self.resolve_store_sources(self.peer_dbs.as_deref(), MAX_PEER_DBS, "WEAVE_PEER_DBS")
    }

    /// Tier-2 v2: resolve the Tier-2 delivery sources as [`StoreSource`]s (local
    /// paths AND remote URLs). Supersedes [`pull_from_paths`](Self::pull_from_paths).
    /// Remote entries carry the shared `pull_token`.
    pub fn pull_from_sources(&self) -> Vec<StoreSource> {
        self.resolve_store_sources(self.pull_from.as_deref(), MAX_PULL_FROM, "WEAVE_PULL_FROM")
    }

    /// Tier-2 consent (decision 5): whether a pulled message from an allow-listed
    /// source also fires the content-free live nudge into THIS session's OWN pane.
    /// **Defaults to `true`** — this is the one place the PRD's default-off is
    /// intentionally overridden. `inject_pulled = false` ⇒ pure queue-only delivery
    /// (the single off-switch). Residual risk: with the default on, any peer you
    /// pull from can, by default, type a capped paste-safe nudge into your live
    /// pane; documented in the config template, README, and ARCHITECTURE.
    pub fn inject_pulled(&self) -> bool {
        self.inject_pulled.unwrap_or(true)
    }

    /// Tier-2 signed-identity strictness (2d): whether an unsigned/unverifiable
    /// pulled intent is dropped (`true`) rather than committed under the advisory
    /// model. **Defaults to `false`** (advisory fallback, identical to 2a–2c). Only
    /// consulted on the pull/commit path of a `--features sign` build; a forged
    /// signature is rejected regardless of this flag.
    ///
    /// Superseded for the decision-table path by
    /// [`strict_verify_override`](Self::strict_verify_override) (which preserves the
    /// tri-state), but kept as the collapsed-bool accessor and exercised by config
    /// unit tests, so `allow(dead_code)` marks the intentional retention.
    #[allow(dead_code)]
    pub fn strict_verify(&self) -> bool {
        self.strict_verify.unwrap_or(false)
    }

    /// The TRI-STATE strict-verify override (2d): `Some(true)` ⇒ user forced strict
    /// everywhere; `Some(false)` ⇒ user disabled strict (advisory everywhere for the
    /// unsigned/unknown path — NOT a license to admit a revoked key's signed message,
    /// R1); `None` ⇒ no override, so the trust-set-aware default decides per sender.
    /// Preserves the forced-global semantics that `strict_verify()` collapses to a
    /// bool. Only consulted on the pull path of a `--features sign` build.
    pub fn strict_verify_override(&self) -> Option<bool> {
        self.strict_verify
    }

    /// The validated, deduplicated TRUST SET (2d): trusted sender fingerprints
    /// (`SHA256:<full-64-hex>`) or full pubkey hex strings. Trims blanks, rejects any
    /// entry with a NUL/control char or longer than [`MAX_FP_ENTRY_LEN`], dedups
    /// preserving first-seen order, and caps the count at [`MAX_TRUST`] with a stderr
    /// note. Default (no env, no config) ⇒ `[]` ⇒ no trust set ⇒ identical-to-today.
    pub fn trust_set(&self) -> Vec<String> {
        resolve_fp_list(self.trust.as_deref(), "WEAVE_TRUST")
    }

    /// The validated, deduplicated REVOCATION LIST (2d), same discipline as
    /// [`trust_set`](Self::trust_set). A fingerprint here causes a signature
    /// verifying against it to be REJECTED unconditionally (R1).
    pub fn revoked_set(&self) -> Vec<String> {
        resolve_fp_list(self.revoked.as_deref(), "WEAVE_REVOKED")
    }

    /// Is a trust set CONFIGURED (i.e. the validated [`trust_set`](Self::trust_set)
    /// is non-empty)? When configured, a trusted sender is verified strictly by
    /// default; when not, every sender keeps the advisory model (unsigned operation
    /// unchanged from today). Exercised by config unit tests and available to the
    /// sign path; the per-intent decision uses `VerifyPolicy::trust_configured`, so
    /// `allow(dead_code)` marks the intentional retention of this public accessor.
    #[allow(dead_code)]
    pub fn trust_set_configured(&self) -> bool {
        !self.trust_set().is_empty()
    }

    /// The validated, deduplicated `allow_inject_from` subset (when set). Resolved
    /// with the SAME discipline as [`pull_from_paths`] (trim, NUL-reject, drop the
    /// local db, canonical-dedup, cap at [`MAX_PULL_FROM`]). `None` ⇒ "same as the
    /// pull set" and this returns `None` so the caller treats every pull source as
    /// inject-eligible. See [`inject_allowed_from`](Self::inject_allowed_from).
    ///
    /// Transitional Local-only wrapper (superseded by
    /// [`allow_inject_from_sources`](Self::allow_inject_from_sources)); kept for
    /// bisectability and exercised by config unit tests.
    #[allow(dead_code)]
    pub fn allow_inject_from_paths(&self) -> Option<Vec<PathBuf>> {
        self.allow_inject_from
            .as_deref()
            .map(|raw| self.resolve_store_list(Some(raw), MAX_PULL_FROM, "WEAVE_ALLOW_INJECT_FROM"))
    }

    /// Is `source` permitted to trigger the consent nudge? When
    /// `allow_inject_from` is unset, EVERY pull source is inject-eligible (the
    /// recommended "same as pull_from" default — being on the pull list is already
    /// the higher trust grant). When set, the source must be in it (canonical-aware
    /// comparison), so the list NARROWS the inject-eligible set to a subset of the
    /// pull set. This gate is checked caller-side, AFTER `inject_pulled()`, so a
    /// non-eligible source can never cause a keystroke in this pane.
    ///
    /// Transitional Local-path gate (superseded by
    /// [`inject_allowed_from_source`](Self::inject_allowed_from_source)); kept for
    /// bisectability and exercised by config unit tests.
    #[allow(dead_code)]
    pub fn inject_allowed_from(&self, source: &std::path::Path) -> bool {
        let allow = match self.allow_inject_from_paths() {
            // Unset ⇒ same as pull set: every pulled source is eligible.
            None => return true,
            Some(list) => list,
        };
        let key = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
        allow.iter().any(|p| {
            let pk = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            pk == key
        })
    }

    /// Shared resolver for a list of foreign store paths (used by both
    /// [`peer_db_paths`] and [`pull_from_paths`]): trims blanks, rejects any entry
    /// containing a NUL byte (an injection canary that cannot be a real path),
    /// drops the local [`db_path`](Self::db_path) (canonical-aware) so the local
    /// store is never opened twice, deduplicates canonically while preserving
    /// first-seen order, and caps the count at `cap` with a one-line stderr note
    /// when truncating. `list_label` names the source list for that note.
    ///
    /// Transitional path-only resolver kept alongside
    /// [`resolve_store_sources`](Self::resolve_store_sources) for bisectability; used
    /// only by the Local-only `*_paths` wrappers, exercised via config unit tests.
    #[allow(dead_code)]
    fn resolve_store_list(
        &self,
        raw: Option<&[String]>,
        cap: usize,
        list_label: &str,
    ) -> Vec<PathBuf> {
        let raw = match raw {
            Some(v) => v,
            None => return Vec::new(),
        };
        let local = self.db_path();
        // Canonicalize the local path once for comparison; fall back to the raw
        // path if it does not exist yet (canonicalize requires existence).
        let local_canon = std::fs::canonicalize(&local).unwrap_or_else(|_| local.clone());

        let mut out: Vec<PathBuf> = Vec::new();
        // Track seen canonical keys so `./messages.db` and its absolute form do
        // not both slip through.
        let mut seen: Vec<PathBuf> = Vec::new();
        for entry in raw {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Reject a NUL byte before constructing a PathBuf used to open a file.
            if trimmed.contains('\0') {
                eprintln!("[weave] skipping invalid {list_label} entry (contains NUL byte)");
                continue;
            }
            let path = PathBuf::from(trimmed);
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            // Never read the local store twice.
            if key == local_canon {
                continue;
            }
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(path);
        }
        if out.len() > cap {
            eprintln!(
                "[weave] {} {list_label} stores configured; capping at {cap}",
                out.len()
            );
            out.truncate(cap);
        }
        out
    }

    /// Tier-2 v2 resolver paralleling [`resolve_store_list`](Self::resolve_store_list)
    /// but yielding [`StoreSource`]s (local paths AND remote URLs). Same discipline:
    /// trim blanks, reject any entry containing a NUL byte, preserve first-seen order,
    /// cap at `cap` with a one-line stderr note. Source-kind-specific handling:
    ///
    /// - **Local** entries: canonicalize for dedup, drop any equal to the local
    ///   [`db_path`](Self::db_path) (never read the local store twice) — exactly as
    ///   the path resolver does.
    /// - **Remote** entries: dedup by the trailing-slash-normalized URL string (a URL
    ///   is NEVER `std::fs::canonicalize`'d), never compared to the local `db_path`,
    ///   and carry the shared sanitized `pull_token` (when set). The token is the
    ///   PARSED secret; it is redacted in `StoreSource`'s Debug.
    ///
    /// Note: remote entries are PARSED on every build (config is backend-agnostic);
    /// the loud "requires --features libsql" rejection happens at the store seam on
    /// the default sqlite build, never here.
    fn resolve_store_sources(
        &self,
        raw: Option<&[String]>,
        cap: usize,
        list_label: &str,
    ) -> Vec<StoreSource> {
        self.resolve_store_sources_with_tiers(raw, cap, list_label)
            .into_iter()
            .map(|(src, _tiers)| src)
            .collect()
    }

    /// Tier-aware sibling of [`resolve_store_sources`](Self::resolve_store_sources):
    /// returns each resolved source paired with the token-free [`PullTokenTier`] that
    /// resolved for it (`PullTokenTier::None` for every Local source — locals carry
    /// no token). The per-source token precedence (label-env → shared → none) is
    /// applied inside the `Remote` arm. The shared token is sanitized ONCE before the
    /// loop. The label is consumed here ONLY to look up its env var and to classify
    /// the tier; it NEVER travels on `StoreSource` and is never logged with the token.
    ///
    /// `resolve_store_sources` discards the tiers; `doctor` keeps them for
    /// observability. Every Local source pairs with `RemoteTiers { token: None,
    /// timeout: Default }` (locals carry neither a token nor a remote timeout).
    fn resolve_store_sources_with_tiers(
        &self,
        raw: Option<&[String]>,
        cap: usize,
        list_label: &str,
    ) -> Vec<(StoreSource, RemoteTiers)> {
        let raw = match raw {
            Some(v) => v,
            None => return Vec::new(),
        };
        let local = self.db_path();
        let local_canon = std::fs::canonicalize(&local).unwrap_or_else(|_| local.clone());
        let shared_token = self.pull_token.as_deref().and_then(sanitize_token);

        let mut out: Vec<(StoreSource, RemoteTiers)> = Vec::new();
        let mut seen_local: Vec<PathBuf> = Vec::new();
        let mut seen_remote: Vec<String> = Vec::new();
        for entry in raw {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.contains('\0') {
                eprintln!("[weave] skipping invalid {list_label} entry (contains NUL byte)");
                continue;
            }
            let (label, source) = parse_labeled_source(trimmed);
            match source {
                StoreSource::Local(path) => {
                    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if key == local_canon {
                        continue; // never read the local store twice
                    }
                    if seen_local.contains(&key) {
                        continue;
                    }
                    seen_local.push(key);
                    out.push((
                        StoreSource::Local(path),
                        RemoteTiers {
                            token: PullTokenTier::None,
                            timeout: PullTimeoutTier::Default,
                        },
                    ));
                }
                StoreSource::Remote { url, .. } => {
                    let key = normalize_remote_url(&url);
                    if seen_remote.contains(&key) {
                        continue;
                    }
                    seen_remote.push(key);
                    let (token, token_tier) =
                        per_source_token(label.as_deref(), shared_token.as_deref());
                    let (timeout_ms, timeout_tier) = per_source_timeout(label.as_deref());
                    out.push((
                        StoreSource::Remote {
                            url,
                            token,
                            timeout_ms,
                        },
                        RemoteTiers {
                            token: token_tier,
                            timeout: timeout_tier,
                        },
                    ));
                }
            }
        }
        if out.len() > cap {
            eprintln!(
                "[weave] {} {list_label} stores configured; capping at {cap}",
                out.len()
            );
            out.truncate(cap);
        }
        out
    }

    /// Token-FREE per-source token-tier observability for `doctor`: re-resolves the
    /// Tier-1 federation sources (`peer_dbs`, the SAME set `doctor` reports its remote
    /// count over) and returns the [`PullTokenTier`] of every REMOTE source. Locals
    /// are omitted (they carry no token). NEVER returns or logs any token byte. Used
    /// only by `doctor` to render aggregate tier counts that line up with the existing
    /// `remote sources: N configured` count.
    pub fn peer_db_remote_token_tiers(&self) -> Vec<PullTokenTier> {
        self.resolve_store_sources_with_tiers(
            self.peer_dbs.as_deref(),
            MAX_PEER_DBS,
            "WEAVE_PEER_DBS",
        )
        .into_iter()
        .filter(|(src, _)| src.is_remote())
        .map(|(_, tiers)| tiers.token)
        .collect()
    }

    /// `pull_from` analogue of [`peer_db_remote_token_tiers`]: the symmetric token-FREE
    /// per-source token-tier observability over the Tier-2 **delivery** set
    /// (`pull_from`). The two source kinds share ONE label namespace + resolver
    /// ([`resolve_store_sources_with_tiers`]), so a labelled remote selects its
    /// `WEAVE_PULL_TOKEN_<LABEL>` whether it appears in `peer_dbs` or `pull_from`; this
    /// just aggregates the tier over the delivery list instead of the federation list.
    /// Locals are omitted (they carry no token). NEVER returns or logs any token byte
    /// nor the label↔token pairing — only the tier classification.
    ///
    /// `doctor` aggregates the `pull_from` side through
    /// [`federation_health`](Self::federation_health) (one method for both kinds), so
    /// this symmetric accessor is exercised by config unit/integration tests — the
    /// `allow(dead_code)` marks that intentional retention (mirrors the sibling
    /// [`pull_from_remote_timeout_tiers`](Self::pull_from_remote_timeout_tiers)).
    #[allow(dead_code)]
    pub fn pull_from_remote_token_tiers(&self) -> Vec<PullTokenTier> {
        self.resolve_store_sources_with_tiers(
            self.pull_from.as_deref(),
            MAX_PULL_FROM,
            "WEAVE_PULL_FROM",
        )
        .into_iter()
        .filter(|(src, _)| src.is_remote())
        .map(|(_, tiers)| tiers.token)
        .collect()
    }

    /// Token-FREE per-source TIMEOUT observability for `doctor` over the `peer_dbs`
    /// Tier-1 federation set (the SAME set [`peer_db_remote_token_tiers`] reports
    /// over). Returns, for every REMOTE source, its EFFECTIVE timeout (ms) paired with
    /// the [`PullTimeoutTier`] that resolved it. The effective ms is the resolved
    /// per-source/global value, or [`REMOTE_TIMEOUT_MS_DEFAULT`] when the source
    /// resolves to the default tier. Locals are omitted (no remote timeout). Carries
    /// no secret — only a plain integer + a tier classification.
    pub fn peer_db_remote_timeout_tiers(&self) -> Vec<(u64, PullTimeoutTier)> {
        self.remote_timeout_tiers(self.peer_dbs.as_deref(), MAX_PEER_DBS, "WEAVE_PEER_DBS")
    }

    /// `pull_from` analogue of [`peer_db_remote_timeout_tiers`]. The two share the same
    /// LABEL namespace + resolution, so a labelled remote selects its
    /// `WEAVE_PULL_TIMEOUT_MS_<LABEL>` whether it appears in `peer_dbs` or `pull_from`.
    /// `doctor` aggregates over the `peer_dbs` set (matching the token-tier surface);
    /// this sibling exposes the same view over the delivery set for callers/tests that
    /// need the `pull_from` perspective — exercised by config unit/integration tests.
    #[allow(dead_code)]
    pub fn pull_from_remote_timeout_tiers(&self) -> Vec<(u64, PullTimeoutTier)> {
        self.remote_timeout_tiers(self.pull_from.as_deref(), MAX_PULL_FROM, "WEAVE_PULL_FROM")
    }

    /// Shared resolver behind the two `*_remote_timeout_tiers` doctor methods: maps
    /// each REMOTE source to `(effective_ms, tier)`, substituting
    /// [`REMOTE_TIMEOUT_MS_DEFAULT`] for the default tier so `doctor` can report a
    /// concrete effective ms range without re-implementing the store fallback.
    fn remote_timeout_tiers(
        &self,
        raw: Option<&[String]>,
        cap: usize,
        list_label: &str,
    ) -> Vec<(u64, PullTimeoutTier)> {
        self.resolve_store_sources_with_tiers(raw, cap, list_label)
            .into_iter()
            .filter_map(|(src, tiers)| match src {
                StoreSource::Remote { timeout_ms, .. } => Some((
                    timeout_ms.unwrap_or(REMOTE_TIMEOUT_MS_DEFAULT),
                    tiers.timeout,
                )),
                StoreSource::Local(_) => None,
            })
            .collect()
    }

    /// Token-FREE per-source-kind rollup behind [`federation_health`](Self::federation_health):
    /// resolves `raw` ONCE through [`resolve_store_sources_with_tiers`] (the SAME
    /// resolver the apply path uses) and tallies counts + tier classifications +
    /// effective-ms range. NEVER reads or returns a token byte — only the tiers/counts.
    /// Used symmetrically for both `peer_db` and `pull_from`, so the rollup cannot
    /// diverge between the two kinds.
    fn federation_kind_health(
        &self,
        raw: Option<&[String]>,
        cap: usize,
        list_label: &str,
    ) -> FederationKindHealth {
        let resolved = self.resolve_store_sources_with_tiers(raw, cap, list_label);
        let mut h = FederationKindHealth {
            total: resolved.len(),
            ..FederationKindHealth::default()
        };
        for (src, tiers) in &resolved {
            match src {
                StoreSource::Local(_) => h.local += 1,
                StoreSource::Remote { timeout_ms, .. } => {
                    h.remote += 1;
                    match tiers.token {
                        PullTokenTier::PerSourceLabel => h.token_per_source += 1,
                        PullTokenTier::Shared => h.token_shared += 1,
                        PullTokenTier::None => h.token_none += 1,
                    }
                    match tiers.timeout {
                        PullTimeoutTier::PerSourceLabel => h.timeout_per_source += 1,
                        PullTimeoutTier::Global => h.timeout_global += 1,
                        PullTimeoutTier::Default => h.timeout_default += 1,
                    }
                    let ms = timeout_ms.unwrap_or(REMOTE_TIMEOUT_MS_DEFAULT);
                    h.ms_min = Some(h.ms_min.map_or(ms, |m| m.min(ms)));
                    h.ms_max = Some(h.ms_max.map_or(ms, |m| m.max(ms)));
                }
            }
        }
        h
    }

    /// Secret-free federation-health rollup for `doctor` (CLI + MCP). Aggregates BOTH
    /// source kinds symmetrically — `peer_db` Tier-1 federation and `pull_from` Tier-2
    /// delivery — through the SAME [`resolve_store_sources_with_tiers`] the apply path
    /// uses, so resolved/applied/surfaced cannot drift. Carries ONLY counts plus an
    /// effective-ms range per kind (see [`FederationKindHealth`]); NEVER a token byte
    /// nor a label↔token pairing. No network probe: this reads config/env only, while
    /// reachability is layered in by `doctor` from the already-computed
    /// [`crate::store::federation_status`], never recomputed here. Backend-agnostic —
    /// identical on the default sqlite and `--features libsql` builds.
    pub fn federation_health(&self) -> FederationHealth {
        FederationHealth {
            peer_db: self.federation_kind_health(
                self.peer_dbs.as_deref(),
                MAX_PEER_DBS,
                "WEAVE_PEER_DBS",
            ),
            pull_from: self.federation_kind_health(
                self.pull_from.as_deref(),
                MAX_PULL_FROM,
                "WEAVE_PULL_FROM",
            ),
        }
    }

    /// Tier-2 v2 inject-gate over a [`StoreSource`] (the [`StoreSource`] analogue of
    /// [`inject_allowed_from`](Self::inject_allowed_from)). When `allow_inject_from`
    /// is unset, EVERY pull source is inject-eligible. When set, a Local source must
    /// canonical-match a Local entry in the list; a Remote source must URL-match a
    /// Remote entry (trailing-slash-normalized). A Remote source is never matched
    /// against a Local allow entry (or vice versa).
    pub fn inject_allowed_from_source(&self, source: &StoreSource) -> bool {
        let allow = match self.allow_inject_from_sources() {
            None => return true,
            Some(list) => list,
        };
        match source {
            StoreSource::Local(p) => {
                let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                allow.iter().any(|a| match a {
                    StoreSource::Local(ap) => {
                        std::fs::canonicalize(ap).unwrap_or_else(|_| ap.clone()) == key
                    }
                    StoreSource::Remote { .. } => false,
                })
            }
            StoreSource::Remote { url, .. } => {
                let key = normalize_remote_url(url);
                allow.iter().any(|a| match a {
                    StoreSource::Remote { url: au, .. } => normalize_remote_url(au) == key,
                    StoreSource::Local(_) => false,
                })
            }
        }
    }

    /// The validated `allow_inject_from` subset as [`StoreSource`]s, or `None` when
    /// unset ("same as the pull set"). The [`StoreSource`] analogue of
    /// [`allow_inject_from_paths`](Self::allow_inject_from_paths). Public so the MCP
    /// server can carry the resolved gate in its `PullConsent` (the full `Config` is
    /// deliberately not plumbed into `mcp`).
    pub fn allow_inject_from_sources(&self) -> Option<Vec<StoreSource>> {
        self.allow_inject_from.as_deref().map(|raw| {
            self.resolve_store_sources(Some(raw), MAX_PULL_FROM, "WEAVE_ALLOW_INJECT_FROM")
        })
    }

    /// Resolved retention window (seconds) for the opportunistic SessionStart GC.
    /// Falls back to [`DEFAULT_RETENTION_SECS`] when unset; a configured `0` (or
    /// any negative value, clamped to `0`) disables the auto-GC. The caller treats
    /// `0` as "skip the sweep entirely".
    pub fn retention(&self) -> i64 {
        self.retention_secs.unwrap_or(DEFAULT_RETENTION_SECS).max(0)
    }

    /// The configured nudge template, if any. Plumbed into the MCP server so its
    /// live-injection nudges honor the same `nudge_template` the CLI uses (the
    /// template carries `{from}`/`{body}` placeholders; see [`Config::nudge`]).
    /// `None` ⇒ the server uses its built-in default nudge text.
    pub fn nudge_template(&self) -> Option<&str> {
        self.nudge_template.as_deref()
    }

    pub fn backend(&self) -> String {
        // Default to the backend actually compiled in: a libsql-only build (no
        // `sqlite` feature) defaults to libsql rather than erroring on "sqlite".
        self.backend.clone().unwrap_or_else(|| {
            if cfg!(feature = "sqlite") {
                "sqlite".to_string()
            } else {
                "libsql".to_string()
            }
        })
    }

    /// Resolved DB path: config/env override, else the XDG default.
    pub fn db_path(&self) -> PathBuf {
        if let Some(p) = &self.db {
            return PathBuf::from(p);
        }
        default_db_path()
    }

    /// The live-injection nudge text for a message from `from` carrying `body`.
    ///
    /// The default nudge embeds the message body so the recipient sees the actual
    /// content the instant it is pushed into their pane (the persisted copy still
    /// arrives on their next hook drain). A custom `nudge_template` may use the
    /// `{from}` and `{body}` placeholders; a template without `{body}` simply
    /// omits the live body (e.g. a quiet "you have mail" ping).
    pub fn nudge(&self, from: &str, body: &str) -> String {
        match &self.nudge_template {
            Some(t) => t.replace("{from}", from).replace("{body}", body),
            None => format!("[weave] message from {from}: {body} (run weave_inbox to read)"),
        }
    }
}

fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Parse a boolean env value with the usual truthy/falsy spellings (case- and
/// whitespace-insensitive). Returns `None` for anything unrecognized so the
/// caller can leave the config/default untouched rather than silently flip it.
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The commented scaffold written by `weave config init`. Every key is commented
/// out so the file is a documented template that still loads as an empty config
/// (all fields default) until the user opts into a setting. Keeping the template
/// here — next to the `Config` struct it documents — means the two cannot drift
/// silently; a new field should be mirrored as a commented line below.
pub const CONFIG_TEMPLATE: &str = "\
# weave configuration — ~/.config/weave/config.toml
#
# Every setting below is OPTIONAL and shown commented-out with its default.
# Uncomment and edit only what you want to override. Environment variables
# (WEAVE_SESSION, WEAVE_BACKEND, WEAVE_DB, WEAVE_LIBSQL_URL,
# WEAVE_LIBSQL_AUTH_TOKEN, WEAVE_PULL_TOKEN, WEAVE_PULL_TOKEN_<LABEL>) take
# precedence over anything set here.

# Default identity for this machine/session. When unset, weave falls back to
# the basename of the current directory (a *guess* that never marks mail read).
# Set this so presence and read-tracking are reliable.
# session = \"desktop\"

# Storage backend: \"sqlite\" (default, bundled) or \"libsql\" (cross-machine sync).
# The libsql backend must be compiled in (`--no-default-features --features libsql`).
# backend = \"sqlite\"

# Override the message database path. Default (sqlite): the XDG data dir,
# i.e. ~/.local/share/weave/messages.db. For a LOCAL libsql backend this IS the
# file path; it is ignored only when libsql_url (a remote) is set.
# db = \"/path/to/messages.db\"

# Live-injection nudge pushed into a peer's pane the instant a message is sent.
# Placeholders: {from} (sender) and {body} (message text). Omit {body} for a
# quiet \"you have mail\" ping that carries no content.
# nudge_template = \"[weave] message from {from}: {body} (run weave_inbox to read)\"

# Auto-retention: at SessionStart weave opportunistically deletes messages older
# than this many seconds (best-effort; failures are ignored). Default 2592000
# (30 days), matching the `weave gc` default. Set 0 to disable the auto-sweep and
# keep messages until you run `weave gc` yourself. Overridable via WEAVE_RETENTION_SECS.
# retention_secs = 2592000

# Remote libSQL/Turso endpoint (only used when backend = \"libsql\").
# libsql_url = \"libsql://your-db.turso.io\"

# Auth token for the remote libSQL endpoint. Treat as a secret; weave redacts it
# from debug output. Prefer the WEAVE_LIBSQL_AUTH_TOKEN env var over storing it here.
# libsql_auth_token = \"...\"

# Federation (READ-ONLY): additional store files to aggregate peers/sessions
# from, so `weave peers`/`weave sessions` can SEE sessions living in other
# projects' stores. These are opened read-only and NEVER written. Default empty
# (single-store). Overridable via WEAVE_PEER_DBS (comma- or path-separated).
# Foreign entries are origin-tagged in the listings; you cannot send across
# stores (Tier 1 is read-only). Capped at 16 stores.
# peer_dbs = [\"/path/to/other-project/messages.db\"]

# Cross-store DELIVERY sources (Tier-2): store files this session will PULL
# directed messages from and COMMIT into its own inbox on its next drain. This is
# a STRICTLY HIGHER trust grant than peer_dbs (which is read-only visibility):
# only a source listed here may deliver a message into your inbox, and only the
# RECEIVER (you) ever writes your own store — a sender deposits an intent into its
# OWN outbox, which you pull read-only. Delivery is next-drain (pull-latency-
# bound), not instant. Default empty (no cross-store delivery). Overridable via
# WEAVE_PULL_FROM (comma- or path-separated). Capped at 16 stores. A path may
# appear in both peer_dbs and pull_from. A REMOTE libSQL/Turso source (a URL with
# scheme libsql:// | https:// | wss://) is also accepted here — it is opened
# READ-ONLY and weave NEVER writes it (owner-only-writes holds cross-machine).
# REMOTE SOURCES REQUIRE a --features libsql build; on the default sqlite build a
# remote entry is skipped with a clear stderr note (local sources still work).
# pull_from = [\"/path/to/other-project/messages.db\", \"libsql://shared-db.turso.io\"]

# PER-SOURCE auth tokens: prefix a REMOTE entry with `LABEL=` to select a distinct
# token via the env var WEAVE_PULL_TOKEN_<LABEL> (label uppercased; charset
# [A-Za-z0-9_]). The LABEL is NOT a secret (it only names which env var holds the
# token), so inlining it is safe. Token precedence per remote source:
# WEAVE_PULL_TOKEN_<LABEL> -> shared pull_token / WEAVE_PULL_TOKEN -> none.
# A label only applies to a REMOTE URL; `LABEL=/local/path` is treated as a literal
# local path. Example (two remotes with separate tokens, plus an unlabelled one that
# falls back to the shared pull_token):
# pull_from = [\"PROD=libsql://prod-db.turso.io\", \"STAGE=libsql://stage-db.turso.io\", \"libsql://shared-db.turso.io\"]

# SHARED auth token for REMOTE pull/federation sources (Tier-2 v2). Applied to every
# libsql://, https:// or wss:// source above that does NOT resolve a per-source
# WEAVE_PULL_TOKEN_<LABEL>. Treat as a SECRET; weave redacts it from debug output.
# Prefer the WEAVE_PULL_TOKEN env var over storing it here. RECOMMENDED: use a
# SERVER-ENFORCED read-only Turso token
# (`turso db tokens create <db> --read-only`) so the source is read-only at the
# server, not just by weave's client-side guards.
# pull_token = \"...\"

# REMOTE-CALL TIMEOUT (ms) for each remote pull/federation source's connect + read.
# This is NOT a config-file key; it is supplied via env. A per-source override rides
# the SAME LABEL namespace as the per-source token, so one `LABEL=` prefix selects
# BOTH that source's token and its timeout — and it applies to remotes in EITHER
# pull_from OR peer_dbs. Precedence per remote source:
#   WEAVE_PULL_TIMEOUT_MS_<LABEL>  (per-source, label uppercased)
#     -> WEAVE_PULL_TIMEOUT_MS      (global)
#     -> 5000 (default).
# The value is parsed as a positive integer and CLAMPED to [50, 600000] ms; a
# 0/unparsable/out-of-range value FALLS THROUGH to the next tier (the bound is NEVER
# disabled — an unbounded remote could hang a drain). `weave doctor` reports the
# per-source / global / default tier counts and the effective ms range (never a
# token). Example: WEAVE_PULL_TIMEOUT_MS_PROD=250 WEAVE_PULL_TIMEOUT_MS=1000

# Consent for live injection on a PULLED cross-store message (DEFAULT: true).
# When a pull commits a message from an allow-listed source, weave ALSO fires a
# content-free paste-safe nudge (\"check your inbox\") into YOUR OWN pane — never a
# foreign pane, and never the message body. RESIDUAL RISK: with this on (the
# default), any peer you pull from can, by default, type a capped nudge into your
# live pane. Set false for pure queue-only delivery (the message still arrives in
# your inbox on the next drain; only the live nudge is suppressed). Overridable via
# WEAVE_INJECT_PULLED.
# inject_pulled = true

# OPTIONAL finer gate: restrict which pull sources may trigger the consent nudge.
# When unset, EVERY pull_from source is inject-eligible (same as the pull set —
# being on the pull list is already the higher trust grant). When set, only a
# source listed here injects; the others still deliver to your inbox, just without
# the live nudge. Use this to NARROW the inject set to a trusted subset.
# Overridable via WEAVE_ALLOW_INJECT_FROM (comma- or path-separated). Capped at 16.
# allow_inject_from = [\"/path/to/other-project/messages.db\"]

# Signed sender identity strictness (only meaningful in a `--features sign` build).
# When true, a pulled cross-store intent that is UNSIGNED or cannot be verified
# against the sender's registered public key is DROPPED instead of committed under
# the advisory allowlist model. A TAMPERED/FORGED signature is always rejected
# regardless of this setting. Default false (advisory fallback — unsigned intents
# still deliver). See `weave key`. Overridable via WEAVE_STRICT_VERIFY.
# Tri-state: unset ⇒ the trust-set-aware default decides per sender (a TRUSTED
# sender is verified strictly, others stay advisory); true ⇒ force strict for every
# sender; false ⇒ disable strict for the unsigned/unknown path. A REVOKED key's
# SIGNED message is ALWAYS rejected even with false (the toggle never re-admits it).
# strict_verify = false

# TRUST SET (only with --features sign): trusted sender fingerprints. A sender
# whose registered public key's fingerprint is listed here is verified STRICTLY —
# a missing or bad signature from them is REJECTED, not warned. Senders NOT in the
# list keep the advisory model (unsigned operation unchanged). Empty (the default)
# ⇒ no trust set ⇒ behavior identical to a no-trust-set build. An entry is a full
# fingerprint `SHA256:<64-hex>` (from `weave key fingerprint`) OR a full pubkey hex;
# the truncated display form is NEVER trusted. Trusting a sender first requires
# registering their key: `weave key add <identity> <pubkey>`. Overridable via
# WEAVE_TRUST (comma- or whitespace-separated). Capped at 64 entries.
# trust = [\"SHA256:0123...\"]

# REVOCATION LIST (only with --features sign): fingerprints whose signatures are no
# longer accepted. A signature that verifies against a revoked key is REJECTED
# UNCONDITIONALLY — even when strict_verify = false. Same entry forms as `trust`.
# Empty (the default) ⇒ nothing revoked. Overridable via WEAVE_REVOKED. Capped at 64.
# revoked = [\"SHA256:dead...\"]

# CIRCLE (P4): the visibility-scoping group this session belongs to. `weave peers`/
# `sessions`/`scan` default to YOUR circle; pass --all-circles (or circle='*') to go
# mesh-wide. An orchestrator (see `weave orchestrator claim`) defaults to mesh-wide
# visibility. A circle is a label (ASCII [A-Za-z0-9_-], <=64), NOT a secret or path.
# Default (unset) ⇒ \"default\", so a single-circle deployment behaves as before.
# Overridable via WEAVE_CIRCLE.
# circle = \"default\"
";

/// Outcome of `weave config init`, so the CLI can report precisely what happened
/// (created vs. left untouched) without re-statting the file.
pub enum ConfigInit {
    /// A fresh config was written at this path.
    Created(PathBuf),
    /// A config already existed here and was left untouched (never overwritten).
    Existed(PathBuf),
}

/// Scaffold a commented `config.toml` at [`config_path`], creating parent dirs as
/// needed. NEVER overwrites an existing file — an existing config is returned as
/// [`ConfigInit::Existed`] so the user's settings (and any secrets) are safe.
pub fn init_config_file() -> std::io::Result<ConfigInit> {
    let path = config_path();
    if path.exists() {
        return Ok(ConfigInit::Existed(path));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // The config may hold a libSQL auth token — keep the dir private (0700).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    // create_new = true makes this atomic against a racing writer: if another
    // process creates the file between the exists() check and here, we fail with
    // AlreadyExists rather than clobbering it. mode(0600) so a token in the file is
    // never world/group readable.
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(mut f) => {
            f.write_all(CONFIG_TEMPLATE.as_bytes())?;
            Ok(ConfigInit::Created(path))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(ConfigInit::Existed(path)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scaffold must parse as valid TOML (every real line is commented, so it
    /// deserializes to an all-default Config) — a malformed template would write a
    /// file weave then refuses to load.
    #[test]
    fn template_is_valid_empty_toml() {
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template parses as TOML");
        assert!(cfg.session.is_none());
        assert!(cfg.backend.is_none());
        assert!(cfg.db.is_none());
        assert!(cfg.nudge_template.is_none());
        assert!(cfg.libsql_url.is_none());
        assert!(cfg.libsql_auth_token.is_none());
        assert!(cfg.retention_secs.is_none());
        assert!(cfg.peer_dbs.is_none());
        assert!(cfg.pull_from.is_none());
        assert!(cfg.inject_pulled.is_none());
        assert!(cfg.allow_inject_from.is_none());
        assert!(cfg.strict_verify.is_none());
        assert!(cfg.trust.is_none());
        assert!(cfg.revoked.is_none());
        assert!(cfg.pull_token.is_none());
        assert!(cfg.circle.is_none());
    }

    /// Every documented placeholder the nudge renderer understands should appear in
    /// the template's nudge example, so the docs and the code agree.
    #[test]
    fn template_documents_nudge_placeholders() {
        assert!(CONFIG_TEMPLATE.contains("{from}"));
        assert!(CONFIG_TEMPLATE.contains("{body}"));
    }

    /// Each real config field should have a (commented) line in the template, so a
    /// newly-added field is not silently left undocumented.
    #[test]
    fn template_mentions_every_field() {
        for key in [
            "session",
            "backend",
            "db",
            "nudge_template",
            "libsql_url",
            "libsql_auth_token",
            "retention_secs",
            "peer_dbs",
            "pull_from",
            "inject_pulled",
            "allow_inject_from",
            "strict_verify",
            "trust",
            "revoked",
            "pull_token",
            "circle",
        ] {
            assert!(
                CONFIG_TEMPLATE.contains(key),
                "template is missing config key {key:?}"
            );
        }
    }

    /// `Config::circle()` (P4): unset ⇒ "default"; a valid value passes through; an
    /// invalid (metachar/oversized) value falls back to "default" (sanitize at the
    /// seam, never store/return a raw untrusted token).
    #[test]
    fn circle_resolves_default_passthrough_and_sanitize() {
        let with_circle = |c: Option<String>| Config {
            circle: c,
            ..Config::default()
        };
        assert_eq!(Config::default().circle(), "default");
        assert_eq!(with_circle(Some("team-a".to_string())).circle(), "team-a");
        assert_eq!(with_circle(Some(String::new())).circle(), "default");
        assert_eq!(with_circle(Some("a/b; rm".to_string())).circle(), "default");
        assert_eq!(
            with_circle(Some("x".repeat(crate::model::MAX_CIRCLE_LEN + 1))).circle(),
            "default"
        );
    }

    /// `retention()` resolves to the 30-day default when unset, honors an explicit
    /// value, treats `0` as "disabled" (kept as 0 so the caller can skip), and
    /// clamps negatives to `0` rather than passing a negative age to gc.
    #[test]
    fn retention_resolves_default_disable_and_clamp() {
        let base = Config::default();
        assert_eq!(base.retention(), DEFAULT_RETENTION_SECS);

        let disabled = Config {
            retention_secs: Some(0),
            ..Config::default()
        };
        assert_eq!(disabled.retention(), 0, "0 disables the auto-sweep");

        let custom = Config {
            retention_secs: Some(3600),
            ..Config::default()
        };
        assert_eq!(custom.retention(), 3600);

        let negative = Config {
            retention_secs: Some(-5),
            ..Config::default()
        };
        assert_eq!(negative.retention(), 0, "negative clamps to disabled");
    }

    /// `peer_db_paths` is default-empty (no federation configured ⇒ `[]`, the
    /// identical-to-today path), and once configured it trims blanks, drops the
    /// local `db_path`, dedups, and caps at `MAX_PEER_DBS`.
    #[test]
    fn peer_db_paths_default_empty_validates_and_caps() {
        // Default ⇒ no federation.
        assert!(Config::default().peer_db_paths().is_empty());

        // Blank entries are dropped; valid distinct entries survive in order.
        let cfg = Config {
            db: Some("/tmp/weave-local-only.db".to_string()),
            peer_dbs: Some(vec![
                "  ".to_string(),
                "/tmp/weave-extra-a.db".to_string(),
                "".to_string(),
                "/tmp/weave-extra-b.db".to_string(),
            ]),
            ..Config::default()
        };
        let paths = cfg.peer_db_paths();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/weave-extra-a.db"),
                PathBuf::from("/tmp/weave-extra-b.db"),
            ],
            "blanks dropped, order preserved"
        );

        // A NUL byte entry is rejected (injection canary), not opened.
        let nul = Config {
            db: Some("/tmp/weave-local-only.db".to_string()),
            peer_dbs: Some(vec!["/tmp/ok.db".to_string(), "/tmp/b\0ad.db".to_string()]),
            ..Config::default()
        };
        assert_eq!(nul.peer_db_paths(), vec![PathBuf::from("/tmp/ok.db")]);

        // The local db_path is never double-counted, and duplicates collapse.
        let dedup = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            peer_dbs: Some(vec![
                "/tmp/weave-self.db".to_string(), // == local, dropped
                "/tmp/dup.db".to_string(),
                "/tmp/dup.db".to_string(), // duplicate
            ]),
            ..Config::default()
        };
        assert_eq!(dedup.peer_db_paths(), vec![PathBuf::from("/tmp/dup.db")]);

        // The count is capped at MAX_PEER_DBS even for a hostile-length list.
        let many: Vec<String> = (0..1000).map(|i| format!("/tmp/peer-{i}.db")).collect();
        let capped = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            peer_dbs: Some(many),
            ..Config::default()
        };
        assert_eq!(capped.peer_db_paths().len(), MAX_PEER_DBS);
    }

    /// `pull_from_paths` is the Tier-2 delivery-source analogue of
    /// `peer_db_paths`: default-empty, trims blanks, drops the local `db_path`,
    /// dedups, and caps at `MAX_PULL_FROM`. It is resolved from the DISTINCT
    /// `pull_from` list, NOT from `peer_dbs` (a read-only visibility source is not
    /// automatically a delivery source).
    #[test]
    fn pull_from_paths_default_empty_validates_and_caps() {
        // Default ⇒ no Tier-2 delivery.
        assert!(Config::default().pull_from_paths().is_empty());

        // pull_from is DISTINCT from peer_dbs: a store only in peer_dbs is not a
        // delivery source.
        let only_peer = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            peer_dbs: Some(vec!["/tmp/visible-only.db".to_string()]),
            ..Config::default()
        };
        assert!(
            only_peer.pull_from_paths().is_empty(),
            "a peer_dbs-only store is not a pull_from delivery source"
        );

        // Blanks dropped, order preserved, local dropped, dups collapse.
        let cfg = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            pull_from: Some(vec![
                "  ".to_string(),
                "/tmp/src-a.db".to_string(),
                "".to_string(),
                "/tmp/weave-self.db".to_string(), // == local, dropped
                "/tmp/src-b.db".to_string(),
                "/tmp/src-b.db".to_string(), // duplicate
            ]),
            ..Config::default()
        };
        assert_eq!(
            cfg.pull_from_paths(),
            vec![
                PathBuf::from("/tmp/src-a.db"),
                PathBuf::from("/tmp/src-b.db"),
            ]
        );

        // NUL byte rejected (injection canary).
        let nul = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            pull_from: Some(vec!["/tmp/ok.db".to_string(), "/tmp/b\0ad.db".to_string()]),
            ..Config::default()
        };
        assert_eq!(nul.pull_from_paths(), vec![PathBuf::from("/tmp/ok.db")]);

        // Capped at MAX_PULL_FROM.
        let many: Vec<String> = (0..1000).map(|i| format!("/tmp/src-{i}.db")).collect();
        let capped = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            pull_from: Some(many),
            ..Config::default()
        };
        assert_eq!(capped.pull_from_paths().len(), MAX_PULL_FROM);
    }

    /// `inject_pulled()` is the decision-5 master toggle: DEFAULT ON when unset,
    /// honoring an explicit false (the single off-switch) and an explicit true.
    #[test]
    fn inject_pulled_defaults_on_and_honors_toggle() {
        // Decision 5: unset ⇒ ON (the PRD's default-off is intentionally flipped).
        assert!(Config::default().inject_pulled());

        let off = Config {
            inject_pulled: Some(false),
            ..Config::default()
        };
        assert!(!off.inject_pulled(), "false ⇒ pure queue-only");

        let on = Config {
            inject_pulled: Some(true),
            ..Config::default()
        };
        assert!(on.inject_pulled());
    }

    /// `strict_verify()` defaults OFF (advisory fallback) and honors an explicit
    /// toggle — the 2d signed-identity strictness flag.
    #[test]
    fn strict_verify_defaults_off_and_honors_toggle() {
        assert!(
            !Config::default().strict_verify(),
            "default ⇒ advisory fallback"
        );
        let strict = Config {
            strict_verify: Some(true),
            ..Config::default()
        };
        assert!(strict.strict_verify());
        let off = Config {
            strict_verify: Some(false),
            ..Config::default()
        };
        assert!(!off.strict_verify());
    }

    /// `strict_verify_override()` is the TRI-STATE the decision table consults:
    /// `None` unset, `Some(true)` forced, `Some(false)` disabled.
    #[test]
    fn strict_verify_override_is_tri_state() {
        assert_eq!(Config::default().strict_verify_override(), None);
        assert_eq!(
            Config {
                strict_verify: Some(true),
                ..Config::default()
            }
            .strict_verify_override(),
            Some(true)
        );
        assert_eq!(
            Config {
                strict_verify: Some(false),
                ..Config::default()
            }
            .strict_verify_override(),
            Some(false)
        );
    }

    /// `trust_set`/`revoked_set` default empty, trim blanks, drop control-char and
    /// over-long entries, dedup preserving order, and cap at `MAX_TRUST`;
    /// `trust_set_configured()` tracks non-emptiness.
    #[test]
    fn trust_and_revoked_sets_parse_cap_dedup() {
        // Default ⇒ empty, no trust set configured.
        assert!(Config::default().trust_set().is_empty());
        assert!(Config::default().revoked_set().is_empty());
        assert!(!Config::default().trust_set_configured());

        let cfg = Config {
            trust: Some(vec![
                "  ".to_string(),
                "SHA256:aaaa".to_string(),
                "".to_string(),
                "SHA256:bbbb".to_string(),
                "SHA256:aaaa".to_string(),        // dup dropped
                "bad\u{7}entry".to_string(),      // control char dropped
                "x".repeat(MAX_FP_ENTRY_LEN + 1), // over-long dropped
            ]),
            revoked: Some(vec!["SHA256:dead".to_string()]),
            ..Config::default()
        };
        assert_eq!(
            cfg.trust_set(),
            vec!["SHA256:aaaa".to_string(), "SHA256:bbbb".to_string()],
            "blanks/dups/control/over-long dropped, order preserved"
        );
        assert!(cfg.trust_set_configured());
        assert_eq!(cfg.revoked_set(), vec!["SHA256:dead".to_string()]);

        // Cap at MAX_TRUST.
        let many: Vec<String> = (0..(MAX_TRUST + 10))
            .map(|i| format!("SHA256:{i:04x}"))
            .collect();
        let capped = Config {
            trust: Some(many),
            ..Config::default()
        };
        assert_eq!(capped.trust_set().len(), MAX_TRUST, "capped at MAX_TRUST");
    }

    /// `inject_allowed_from` gate: unset `allow_inject_from` ⇒ every source is
    /// inject-eligible ("same as pull_from"); when set, only listed sources are
    /// eligible and a non-listed source is NEVER eligible (no keystroke path).
    #[test]
    fn inject_allowed_from_gates_to_subset() {
        // Unset ⇒ same as pull set: any source is eligible.
        let same_as_pull = Config::default();
        assert!(same_as_pull.inject_allowed_from(std::path::Path::new("/tmp/any-src.db")));

        // Set ⇒ only the listed subset is eligible; others deliver but never inject.
        let narrowed = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            allow_inject_from: Some(vec!["/tmp/trusted.db".to_string()]),
            ..Config::default()
        };
        assert!(narrowed.inject_allowed_from(std::path::Path::new("/tmp/trusted.db")));
        assert!(
            !narrowed.inject_allowed_from(std::path::Path::new("/tmp/other.db")),
            "a source not in allow_inject_from never injects"
        );

        // A blank/empty allow_inject_from list resolves to an empty set ⇒ nothing
        // is eligible (an explicit-but-empty narrow, distinct from unset).
        let empty = Config {
            allow_inject_from: Some(vec![]),
            ..Config::default()
        };
        assert!(!empty.inject_allowed_from(std::path::Path::new("/tmp/any.db")));
    }

    // ---------------------------------------------------------------------
    // Tier-2 v2 — StoreSource classification + resolution + token hygiene.
    // ---------------------------------------------------------------------

    /// `classify_source` maps every recognized remote scheme to `Remote` (with the
    /// URL preserved verbatim, no canonicalization, no token attached) and EVERY
    /// other shape — bare name, `./relative`, `/absolute`, `~`, a Windows-ish path —
    /// to `Local`.
    #[test]
    fn classify_source_recognizes_remote_schemes_else_local() {
        for url in [
            "libsql://db.turso.io",
            "https://db.turso.io",
            "http://127.0.0.1:8080",
            "wss://db.turso.io",
            "ws://localhost:9000",
        ] {
            match classify_source(url) {
                StoreSource::Remote {
                    url: got,
                    token,
                    timeout_ms,
                } => {
                    assert_eq!(got, url, "URL preserved verbatim (no canonicalization)");
                    assert!(token.is_none(), "classify never attaches a token");
                    assert!(timeout_ms.is_none(), "classify never attaches a timeout");
                }
                StoreSource::Local(_) => panic!("{url:?} must classify Remote"),
            }
        }

        for path in [
            "messages.db",
            "./relative/x.db",
            "/abs/x.db",
            "~/x.db",
            "C:\\weave\\x.db",
            "libsql-but-no-scheme.db", // a prefix that is NOT a full scheme
            "ftp://not-a-supported-scheme", // unsupported scheme ⇒ treated as a path
        ] {
            match classify_source(path) {
                StoreSource::Local(p) => assert_eq!(p, PathBuf::from(path)),
                StoreSource::Remote { .. } => panic!("{path:?} must classify Local"),
            }
        }
    }

    /// `resolve_store_sources` (via `pull_from_sources`) handles a MIXED local+remote
    /// list: trims blanks, rejects NUL, preserves first-seen order, dedups remotes by
    /// exact URL (trailing-slash-normalized), dedups+canonicalizes locals, drops a
    /// local equal to `db_path`, NEVER compares a remote to `db_path`, and attaches
    /// the shared `pull_token` to every remote.
    #[test]
    fn resolve_store_sources_mixed_local_and_remote() {
        let cfg = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            pull_token: Some("secret-jwt".to_string()),
            pull_from: Some(vec![
                "  ".to_string(),                   // blank → dropped
                "/tmp/local-a.db".to_string(),      // local, kept
                "libsql://h.turso.io".to_string(),  // remote, kept
                "libsql://h.turso.io/".to_string(), // trailing-slash dup of prev → dropped
                "/tmp/weave-self.db".to_string(),   // == db_path → dropped
                "https://h2.turso.io".to_string(),  // remote, kept
                "https://h2.turso.io".to_string(),  // exact dup → dropped
            ]),
            ..Config::default()
        };
        let sources = cfg.pull_from_sources();
        assert_eq!(
            sources.len(),
            3,
            "blanks/dups/local-self all dropped: {sources:?}"
        );

        // First-seen order preserved: local, libsql remote, https remote.
        assert_eq!(
            sources[0],
            StoreSource::Local(PathBuf::from("/tmp/local-a.db"))
        );
        match &sources[1] {
            StoreSource::Remote { url, token, .. } => {
                assert_eq!(url, "libsql://h.turso.io");
                assert_eq!(
                    token.as_deref(),
                    Some("secret-jwt"),
                    "shared token attached"
                );
            }
            other => panic!("expected libsql remote, got {other:?}"),
        }
        match &sources[2] {
            StoreSource::Remote { url, token, .. } => {
                assert_eq!(url, "https://h2.turso.io");
                assert_eq!(token.as_deref(), Some("secret-jwt"));
            }
            other => panic!("expected https remote, got {other:?}"),
        }
    }

    /// The source count cap (`MAX_PULL_FROM`) bounds a hostile remote-only list, and
    /// a NUL byte in a remote entry is rejected as an injection canary.
    #[test]
    fn resolve_store_sources_caps_and_rejects_nul() {
        let many: Vec<String> = (0..1000)
            .map(|i| format!("libsql://h{i}.turso.io"))
            .collect();
        let capped = Config {
            pull_from: Some(many),
            ..Config::default()
        };
        assert_eq!(capped.pull_from_sources().len(), MAX_PULL_FROM);

        let nul = Config {
            pull_from: Some(vec![
                "libsql://ok.turso.io".to_string(),
                "libsql://b\0ad.turso.io".to_string(),
            ]),
            ..Config::default()
        };
        let got = nul.pull_from_sources();
        assert_eq!(got.len(), 1, "NUL entry rejected: {got:?}");
        assert!(
            matches!(&got[0], StoreSource::Remote { url, .. } if url == "libsql://ok.turso.io")
        );
    }

    /// SECRET HYGIENE: the redacting `Debug` for a `Remote` source and for `Config`
    /// NEVER prints the token bytes — the secret substring is absent from `{:?}` —
    /// while the (non-secret) URL is still shown for diagnostics.
    #[test]
    fn store_source_and_config_debug_redact_the_token() {
        const SECRET: &str = "super-secret-turso-jwt-value";

        let remote = StoreSource::Remote {
            url: "libsql://db.turso.io".to_string(),
            token: Some(SECRET.to_string()),
            timeout_ms: None,
        };
        let dbg = format!("{remote:?}");
        assert!(
            !dbg.contains(SECRET),
            "Remote Debug leaked the token: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "Remote Debug should mark redaction: {dbg}"
        );
        assert!(
            dbg.contains("libsql://db.turso.io"),
            "URL shown (not a secret): {dbg}"
        );

        let cfg = Config {
            pull_token: Some(SECRET.to_string()),
            ..Config::default()
        };
        let cdbg = format!("{cfg:?}");
        assert!(
            !cdbg.contains(SECRET),
            "Config Debug leaked pull_token: {cdbg}"
        );
        assert!(
            cdbg.contains("<redacted>"),
            "Config Debug should redact pull_token: {cdbg}"
        );
    }

    /// TOKEN CAP / CONTROL-CHAR REJECTION: an over-long token and a control-char
    /// token are both dropped by `sanitize_token` (so the remote is attempted WITHOUT
    /// a weave-supplied token rather than handing a hostile value to the client), a
    /// well-formed token survives, and the empty token is treated as "none".
    #[test]
    fn pull_token_capped_and_control_chars_rejected() {
        // Over the byte cap ⇒ no token attached to the resolved remote.
        let too_long = "x".repeat(MAX_TOKEN_LEN + 1);
        let cfg_long = Config {
            pull_token: Some(too_long.clone()),
            pull_from: Some(vec!["libsql://h.turso.io".to_string()]),
            ..Config::default()
        };
        let long_srcs = cfg_long.pull_from_sources();
        assert!(
            matches!(&long_srcs[0], StoreSource::Remote { token: None, .. }),
            "over-cap token must be rejected (None): {long_srcs:?}"
        );

        // A control char (newline) ⇒ rejected.
        let cfg_ctrl = Config {
            pull_token: Some("good\nbad".to_string()),
            pull_from: Some(vec!["libsql://h.turso.io".to_string()]),
            ..Config::default()
        };
        let ctrl_srcs = cfg_ctrl.pull_from_sources();
        assert!(
            matches!(&ctrl_srcs[0], StoreSource::Remote { token: None, .. }),
            "control-char token must be rejected (None): {ctrl_srcs:?}"
        );

        // Exactly at the cap ⇒ accepted.
        let at_cap = "y".repeat(MAX_TOKEN_LEN);
        assert_eq!(sanitize_token(&at_cap).as_deref(), Some(at_cap.as_str()));

        // A clean token survives; empty ⇒ None.
        assert_eq!(sanitize_token("clean-jwt").as_deref(), Some("clean-jwt"));
        assert_eq!(sanitize_token(""), None);
    }

    /// `inject_allowed_from_source` never matches across kinds: a Remote source is
    /// not eligible against a Local allow-entry and vice versa; a Remote matches a
    /// Remote allow-entry by trailing-slash-normalized URL.
    #[test]
    fn inject_allowed_from_source_never_crosses_kinds() {
        let cfg = Config {
            db: Some("/tmp/weave-self.db".to_string()),
            allow_inject_from: Some(vec!["libsql://trusted.turso.io".to_string()]),
            ..Config::default()
        };
        let trusted = StoreSource::Remote {
            url: "libsql://trusted.turso.io/".to_string(), // trailing slash → normalized match
            token: None,
            timeout_ms: None,
        };
        assert!(cfg.inject_allowed_from_source(&trusted));

        let untrusted = StoreSource::Remote {
            url: "libsql://evil.turso.io".to_string(),
            token: None,
            timeout_ms: None,
        };
        assert!(!cfg.inject_allowed_from_source(&untrusted));

        // A Local source is NOT eligible against a Remote-only allow list.
        let local = StoreSource::Local(PathBuf::from("/tmp/trusted.turso.io"));
        assert!(!cfg.inject_allowed_from_source(&local));
    }

    /// The template scaffold documents the new `pull_token` key + the remote
    /// `pull_from` example, and still parses as all-default TOML.
    #[test]
    fn template_documents_pull_token_and_remote_example() {
        assert!(CONFIG_TEMPLATE.contains("pull_token"));
        assert!(
            CONFIG_TEMPLATE.contains("libsql://"),
            "template should show a remote pull_from example"
        );
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template parses");
        assert!(cfg.pull_token.is_none());
    }

    /// `split_source_list` keeps a REMOTE URL whole (its `scheme://` colon must not
    /// be treated as the path-list separator), while still splitting bare paths on
    /// the comma AND the platform separator. This is the regression guard for the
    /// "a `libsql://` entry in `WEAVE_PEER_DBS` gets shredded into `libsql` + `//h`"
    /// bug: a URL must survive the env-list splitter intact.
    #[test]
    fn split_source_list_keeps_urls_whole() {
        // A comma-separated mix: a local path + a remote URL. The URL survives.
        let got = split_source_list("/tmp/a.db,libsql://h.turso.io");
        assert_eq!(got, vec!["/tmp/a.db", "libsql://h.turso.io"]);

        // A URL containing a port colon is also kept whole.
        let got = split_source_list("https://h.turso.io:8080,/tmp/b.db");
        assert_eq!(got, vec!["https://h.turso.io:8080", "/tmp/b.db"]);

        // Blanks dropped; whitespace trimmed.
        let got = split_source_list(" , libsql://h , ");
        assert_eq!(got, vec!["libsql://h"]);

        // Bare paths still split on the platform separator (unix `:`), so existing
        // PATH-style configs keep working.
        #[cfg(not(windows))]
        {
            let got = split_source_list("/tmp/a.db:/tmp/b.db");
            assert_eq!(got, vec!["/tmp/a.db", "/tmp/b.db"]);
        }
    }

    /// End-to-end through `load()`: a remote URL supplied via `WEAVE_PEER_DBS`
    /// resolves to a single `Remote` source (NOT shredded). Guards the env → config
    /// → resolver path for remote URLs. (Serialized via the env so it does not race
    /// other env-reading tests — uses a process-unique var teardown.)
    #[test]
    fn env_peer_dbs_remote_url_resolves_to_one_remote_source() {
        // Drive resolve_store_sources directly with a config built as load() would
        // (env splitting is the unit under test above; here we assert the resolver
        // treats the un-shredded URL as one Remote).
        let cfg = Config {
            peer_dbs: Some(split_source_list("libsql://shared.turso.io")),
            ..Config::default()
        };
        let sources = cfg.peer_db_sources();
        assert_eq!(sources.len(), 1, "one remote source: {sources:?}");
        assert!(matches!(
            &sources[0],
            StoreSource::Remote { url, .. } if url == "libsql://shared.turso.io"
        ));
    }

    // ---------------------------------------------------------------------
    // Per-source pull tokens (WEAVE_PULL_TOKEN_<LABEL>).
    // ---------------------------------------------------------------------

    /// `is_valid_label` accepts a non-empty `[A-Za-z0-9_]` string up to
    /// `MAX_LABEL_LEN`, and rejects empty, over-length, and any other charset.
    #[test]
    fn is_valid_label_charset_and_bounds() {
        for ok in ["A", "a", "A1_b", "PROD", "_", &"x".repeat(MAX_LABEL_LEN)] {
            assert!(is_valid_label(ok), "{ok:?} should be a valid label");
        }
        for bad in [
            "",
            &"x".repeat(MAX_LABEL_LEN + 1),
            "a-b",
            "a b",
            "a.b",
            "a/b",
            "café",
        ] {
            assert!(!is_valid_label(bad), "{bad:?} should be rejected");
        }
    }

    /// `parse_labeled_source` recognizes `LABEL=<remote-url>` (uppercasing the
    /// label), but degrades to a verbatim `classify_source` call for a bare URL, a
    /// local path, a local path containing `=`, an invalid label, and an over-length
    /// label — never silently stripping a bad label to recover a URL.
    #[test]
    fn parse_labeled_source_label_only_on_remote_url() {
        // Valid label + remote URL ⇒ label uppercased, Remote on <rest>.
        let (label, src) = parse_labeled_source("PROD=libsql://prod.turso.io");
        assert_eq!(label.as_deref(), Some("PROD"));
        assert!(matches!(&src, StoreSource::Remote { url, token, .. }
            if url == "libsql://prod.turso.io" && token.is_none()));

        // lowercase label is uppercased for the env lookup.
        let (label, src) = parse_labeled_source("team_a=https://a.turso.io/db");
        assert_eq!(label.as_deref(), Some("TEAM_A"));
        assert!(matches!(&src, StoreSource::Remote { url, .. } if url == "https://a.turso.io/db"));

        // Bare URL ⇒ no label.
        let (label, src) = parse_labeled_source("libsql://shared.turso.io");
        assert!(label.is_none());
        assert!(src.is_remote());

        // Local path (no `=`) ⇒ no label, Local.
        let (label, src) = parse_labeled_source("/tmp/local.db");
        assert!(label.is_none());
        assert_eq!(src, StoreSource::Local(PathBuf::from("/tmp/local.db")));

        // Local path containing `=` whose right side is NOT a remote URL ⇒ verbatim
        // Local (the whole `a=b.db`), NOT a label split.
        let (label, src) = parse_labeled_source("a=b.db");
        assert!(label.is_none());
        assert_eq!(src, StoreSource::Local(PathBuf::from("a=b.db")));

        // Invalid label (space) ⇒ verbatim classify ⇒ Local (no scheme prefix).
        let (label, src) = parse_labeled_source("BAD LABEL=libsql://h");
        assert!(label.is_none());
        assert_eq!(
            src,
            StoreSource::Local(PathBuf::from("BAD LABEL=libsql://h"))
        );

        // Empty label ⇒ verbatim classify ⇒ Local.
        let (label, src) = parse_labeled_source("=libsql://h");
        assert!(label.is_none());
        assert_eq!(src, StoreSource::Local(PathBuf::from("=libsql://h")));

        // Over-length label ⇒ verbatim classify ⇒ Local.
        let over = format!("{}=libsql://h", "x".repeat(MAX_LABEL_LEN + 1));
        let (label, src) = parse_labeled_source(&over);
        assert!(label.is_none());
        assert_eq!(src, StoreSource::Local(PathBuf::from(over)));
    }

    /// `per_source_token` precedence: a set+sane label-env wins; a set-but-rejected
    /// label-env falls THROUGH to the shared token; an unset label-env uses the
    /// shared token; with neither, `None`. Asserts the paired `PullTokenTier`.
    #[test]
    fn per_source_token_precedence() {
        let _g = crate::testenv::lock_env();
        let var = "WEAVE_PULL_TOKEN_PSTEST";
        // RAII: capture+clear prior, restored (removed if absent) on drop.
        let _v = crate::testenv::EnvVarGuard::remove(var);

        // Label-env set + sane ⇒ per-source token, tier PerSourceLabel.
        std::env::set_var(var, "per-source-jwt");
        let (tok, tier) = per_source_token(Some("PSTEST"), Some("shared-jwt"));
        assert_eq!(tok.as_deref(), Some("per-source-jwt"));
        assert_eq!(tier, PullTokenTier::PerSourceLabel);

        // Label-env set but over-cap ⇒ falls through to shared.
        std::env::set_var(var, "x".repeat(MAX_TOKEN_LEN + 1));
        let (tok, tier) = per_source_token(Some("PSTEST"), Some("shared-jwt"));
        assert_eq!(tok.as_deref(), Some("shared-jwt"));
        assert_eq!(tier, PullTokenTier::Shared);

        // Label-env set but control-char ⇒ falls through to shared.
        std::env::set_var(var, "good\nbad");
        let (tok, tier) = per_source_token(Some("PSTEST"), Some("shared-jwt"));
        assert_eq!(tok.as_deref(), Some("shared-jwt"));
        assert_eq!(tier, PullTokenTier::Shared);

        // Label-env unset ⇒ shared.
        std::env::remove_var(var);
        let (tok, tier) = per_source_token(Some("PSTEST"), Some("shared-jwt"));
        assert_eq!(tok.as_deref(), Some("shared-jwt"));
        assert_eq!(tier, PullTokenTier::Shared);

        // No label, shared present ⇒ shared.
        let (tok, tier) = per_source_token(None, Some("shared-jwt"));
        assert_eq!(tok.as_deref(), Some("shared-jwt"));
        assert_eq!(tier, PullTokenTier::Shared);

        // No label, no shared ⇒ none.
        let (tok, tier) = per_source_token(None, None);
        assert!(tok.is_none());
        assert_eq!(tier, PullTokenTier::None);

        // Label set-but-rejected with NO shared ⇒ none (fall-through to None).
        std::env::set_var(var, "bad\nvalue");
        let (tok, tier) = per_source_token(Some("PSTEST"), None);
        assert!(tok.is_none());
        assert_eq!(tier, PullTokenTier::None);
    }

    /// End-to-end through `resolve_store_sources`: a labelled remote picks up its
    /// per-source `WEAVE_PULL_TOKEN_<LABEL>` while a SECOND unlabelled remote in the
    /// same list carries the shared token — proving per-source and shared coexist in
    /// one resolve. The redacted `Debug` of the result still leaks neither token.
    #[test]
    fn resolve_store_sources_per_source_and_shared_coexist() {
        let _g = crate::testenv::lock_env();
        let var = "WEAVE_PULL_TOKEN_PROD";
        let _v = crate::testenv::EnvVarGuard::set(var, "prod-only-jwt");

        let cfg = Config {
            pull_token: Some("shared-jwt".to_string()),
            pull_from: Some(vec![
                "PROD=libsql://prod.turso.io".to_string(),
                "libsql://shared.turso.io".to_string(),
            ]),
            ..Config::default()
        };
        let sources = cfg.pull_from_sources();
        assert_eq!(sources.len(), 2);
        match &sources[0] {
            StoreSource::Remote { url, token, .. } => {
                assert_eq!(url, "libsql://prod.turso.io");
                assert_eq!(token.as_deref(), Some("prod-only-jwt"));
            }
            other => panic!("expected labelled remote, got {other:?}"),
        }
        match &sources[1] {
            StoreSource::Remote { url, token, .. } => {
                assert_eq!(url, "libsql://shared.turso.io");
                assert_eq!(token.as_deref(), Some("shared-jwt"));
            }
            other => panic!("expected unlabelled remote, got {other:?}"),
        }

        // Tier observability (token-free) sees one per-source + one shared.
        let cfg2 = Config {
            peer_dbs: cfg.pull_from.clone(),
            ..cfg.clone()
        };
        let tiers = cfg2.peer_db_remote_token_tiers();
        assert_eq!(
            tiers
                .iter()
                .filter(|t| **t == PullTokenTier::PerSourceLabel)
                .count(),
            1
        );
        assert_eq!(
            tiers
                .iter()
                .filter(|t| **t == PullTokenTier::Shared)
                .count(),
            1
        );

        // Neither token appears in the redacting Debug.
        let dbg = format!("{sources:?}");
        assert!(!dbg.contains("prod-only-jwt"), "leaked per-source: {dbg}");
        assert!(!dbg.contains("shared-jwt"), "leaked shared: {dbg}");
    }

    /// A per-source `WEAVE_PULL_TOKEN_<LABEL>` over the cap / with control chars is
    /// rejected and FALLS THROUGH to the shared token (parity with the shared-token
    /// sanitize behavior), end-to-end through `resolve_store_sources`.
    #[test]
    fn resolve_per_source_token_capped_falls_through_to_shared() {
        let _g = crate::testenv::lock_env();
        let var = "WEAVE_PULL_TOKEN_STAGE";
        let _v = crate::testenv::EnvVarGuard::set(var, &"x".repeat(MAX_TOKEN_LEN + 1));

        let cfg = Config {
            pull_token: Some("shared-jwt".to_string()),
            pull_from: Some(vec!["STAGE=libsql://stage.turso.io".to_string()]),
            ..Config::default()
        };
        let sources = cfg.pull_from_sources();
        assert!(
            matches!(&sources[0], StoreSource::Remote { token, .. } if token.as_deref() == Some("shared-jwt")),
            "over-cap per-source token must fall through to shared: {sources:?}"
        );
    }

    /// `split_source_list` keeps an inline `LABEL=<remote-url>` fragment OPAQUE: the
    /// `:` inside its `scheme://` must NOT shred it on the platform path separator
    /// (regression guard for the env path — the resolver only ever sees a whole
    /// labelled URL). Bare remote URLs stay opaque; locals still split on the sep.
    #[test]
    fn split_source_list_keeps_labelled_remote_opaque() {
        // A labelled remote travels whole (not shredded into `MYDB=libsql` + `//h/db`).
        assert_eq!(
            split_source_list("MYDB=libsql://h.turso.io/db"),
            vec!["MYDB=libsql://h.turso.io/db".to_string()]
        );
        // A bare remote URL is still opaque.
        assert_eq!(
            split_source_list("libsql://h.turso.io/db"),
            vec!["libsql://h.turso.io/db".to_string()]
        );
        // A labelled remote alongside a local entry: only the local participates in
        // any path-sep splitting; the labelled URL stays whole.
        let got = split_source_list("PROD=https://a.turso.io/db,/tmp/x.db");
        assert_eq!(
            got,
            vec![
                "PROD=https://a.turso.io/db".to_string(),
                "/tmp/x.db".to_string()
            ]
        );
        // An invalid-label `=` fragment is NOT treated as opaque (right side is a URL
        // but the label is invalid) — it falls to the normal split, which on unix
        // splits the `:` in the scheme. This matches `parse_labeled_source` degrading
        // it to a verbatim (non-remote) entry anyway, so the post-split entries simply
        // fail to classify as a remote — never a silent labelled remote.
        let bad = split_source_list("bad label=libsql://h");
        assert!(
            !bad.contains(&"bad label=libsql://h".to_string()),
            "an invalid label must not be treated as an opaque remote: {bad:?}"
        );
    }

    /// The template documents the inline `LABEL=` per-source token syntax + the
    /// `WEAVE_PULL_TOKEN_<LABEL>` env var, and still parses as all-default TOML.
    #[test]
    fn template_documents_per_source_label_tokens() {
        assert!(CONFIG_TEMPLATE.contains("WEAVE_PULL_TOKEN_<LABEL>"));
        assert!(CONFIG_TEMPLATE.contains("PROD=libsql://"));
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template parses");
        assert!(cfg.pull_token.is_none());
    }

    // ---------------------------------------------------------------------
    // Per-source remote-call timeout (WEAVE_PULL_TIMEOUT_MS[_<LABEL>]).
    // ---------------------------------------------------------------------

    /// `parse_clamp_timeout`: a sane value passes through; `0`/non-numeric ⇒ `None`
    /// (the caller falls through); below `MIN` clamps UP; above `MAX` clamps DOWN; the
    /// exact bounds are accepted unchanged. NEVER disables the bound.
    #[test]
    fn parse_clamp_timeout_cases() {
        assert_eq!(parse_clamp_timeout("200"), Some(200));
        assert_eq!(
            parse_clamp_timeout("0"),
            None,
            "0 falls through (never disable)"
        );
        assert_eq!(
            parse_clamp_timeout("-5"),
            None,
            "u64 parse fails on negative"
        );
        assert_eq!(parse_clamp_timeout("abc"), None);
        assert_eq!(parse_clamp_timeout(""), None);
        assert_eq!(
            parse_clamp_timeout("10"),
            Some(MIN_TIMEOUT_MS),
            "clamped UP to MIN"
        );
        assert_eq!(
            parse_clamp_timeout("99999999"),
            Some(MAX_TIMEOUT_MS),
            "clamped DOWN to MAX"
        );
        assert_eq!(
            parse_clamp_timeout(&MIN_TIMEOUT_MS.to_string()),
            Some(MIN_TIMEOUT_MS)
        );
        assert_eq!(
            parse_clamp_timeout(&MAX_TIMEOUT_MS.to_string()),
            Some(MAX_TIMEOUT_MS)
        );
        // Surrounding whitespace is trimmed before parsing.
        assert_eq!(parse_clamp_timeout("  250  "), Some(250));
    }

    /// `clamp_watch_interval`: a sane value passes through; `0` clamps UP to the
    /// floor; an enormous value clamps DOWN to the ceiling; the exact bounds are
    /// accepted unchanged; clamping is idempotent.
    #[test]
    fn clamp_watch_interval_cases() {
        assert_eq!(clamp_watch_interval(2), 2, "sane value passes through");
        assert_eq!(
            clamp_watch_interval(0),
            WATCH_INTERVAL_MIN_SECS,
            "0 clamps UP to floor (no busy-spin)"
        );
        assert_eq!(
            clamp_watch_interval(u64::MAX),
            WATCH_INTERVAL_MAX_SECS,
            "huge value clamps DOWN to ceiling"
        );
        assert_eq!(
            clamp_watch_interval(WATCH_INTERVAL_MIN_SECS),
            WATCH_INTERVAL_MIN_SECS
        );
        assert_eq!(
            clamp_watch_interval(WATCH_INTERVAL_MAX_SECS),
            WATCH_INTERVAL_MAX_SECS
        );
        // Idempotent: re-clamping a clamped value is a no-op.
        let once = clamp_watch_interval(0);
        assert_eq!(clamp_watch_interval(once), once);
    }

    /// `per_source_timeout` precedence (mirrors `per_source_token`): a sane label-env
    /// wins (tier `PerSourceLabel`); a set-but-garbage label-env falls THROUGH to the
    /// global (tier `Global`); per-source + global both set ⇒ per-source wins; neither
    /// set ⇒ `(None, Default)`.
    #[test]
    fn per_source_timeout_precedence() {
        let _g = crate::testenv::lock_env();
        let label_var = "WEAVE_PULL_TIMEOUT_MS_TOTEST";
        let global_var = "WEAVE_PULL_TIMEOUT_MS";
        let _vl = crate::testenv::EnvVarGuard::remove(label_var);
        let _vg = crate::testenv::EnvVarGuard::remove(global_var);

        // Neither set ⇒ default tier, no resolved value (store applies its default).
        let (ms, tier) = per_source_timeout(Some("TOTEST"));
        assert!(ms.is_none());
        assert_eq!(tier, PullTimeoutTier::Default);

        // Label-env set + sane ⇒ per-source, clamped, tier PerSourceLabel.
        std::env::set_var(label_var, "250");
        let (ms, tier) = per_source_timeout(Some("TOTEST"));
        assert_eq!(ms, Some(250));
        assert_eq!(tier, PullTimeoutTier::PerSourceLabel);

        // Per-source + global both set ⇒ per-source wins.
        std::env::set_var(global_var, "1000");
        let (ms, tier) = per_source_timeout(Some("TOTEST"));
        assert_eq!(ms, Some(250));
        assert_eq!(tier, PullTimeoutTier::PerSourceLabel);

        // Per-source garbage ⇒ falls THROUGH to the global (tier Global).
        std::env::set_var(label_var, "0");
        let (ms, tier) = per_source_timeout(Some("TOTEST"));
        assert_eq!(ms, Some(1000));
        assert_eq!(tier, PullTimeoutTier::Global);

        // No label, only global set ⇒ Global.
        std::env::remove_var(label_var);
        let (ms, tier) = per_source_timeout(None);
        assert_eq!(ms, Some(1000));
        assert_eq!(tier, PullTimeoutTier::Global);

        // Global garbage + no label ⇒ default tier.
        std::env::set_var(global_var, "notanumber");
        let (ms, tier) = per_source_timeout(None);
        assert!(ms.is_none());
        assert_eq!(tier, PullTimeoutTier::Default);
    }

    /// End-to-end: `resolve_store_sources_with_tiers` carries the clamped per-source
    /// timeout onto `StoreSource::Remote.timeout_ms` and reports the `PerSourceLabel`
    /// timeout tier, while a second unlabelled remote (only the global set) reports the
    /// `Global` tier. The `source_cursor_key`-relevant URL is untouched by the timeout.
    #[test]
    fn resolve_store_sources_carries_per_source_timeout_and_tier() {
        let _g = crate::testenv::lock_env();
        let label_var = "WEAVE_PULL_TIMEOUT_MS_PROD";
        let global_var = "WEAVE_PULL_TIMEOUT_MS";
        let _vl = crate::testenv::EnvVarGuard::set(label_var, "250");
        let _vg = crate::testenv::EnvVarGuard::set(global_var, "1000");

        let cfg = Config {
            pull_from: Some(vec![
                "PROD=libsql://prod.turso.io".to_string(),
                "libsql://shared.turso.io".to_string(),
            ]),
            ..Config::default()
        };
        let resolved = cfg.resolve_store_sources_with_tiers(
            cfg.pull_from.as_deref(),
            MAX_PULL_FROM,
            "WEAVE_PULL_FROM",
        );
        assert_eq!(resolved.len(), 2);

        // Labelled remote: clamped per-source ms + PerSourceLabel tier.
        match &resolved[0] {
            (
                StoreSource::Remote {
                    url, timeout_ms, ..
                },
                tiers,
            ) => {
                assert_eq!(url, "libsql://prod.turso.io");
                assert_eq!(*timeout_ms, Some(250));
                assert_eq!(tiers.timeout, PullTimeoutTier::PerSourceLabel);
            }
            other => panic!("expected labelled remote, got {other:?}"),
        }
        // Unlabelled remote: falls back to the global ms + Global tier.
        match &resolved[1] {
            (
                StoreSource::Remote {
                    url, timeout_ms, ..
                },
                tiers,
            ) => {
                assert_eq!(url, "libsql://shared.turso.io");
                assert_eq!(*timeout_ms, Some(1000));
                assert_eq!(tiers.timeout, PullTimeoutTier::Global);
            }
            other => panic!("expected unlabelled remote, got {other:?}"),
        }

        // The doctor timeout-tier method reports effective ms within [MIN, MAX].
        let tiers = cfg.pull_from_remote_timeout_tiers();
        assert_eq!(tiers.len(), 2);
        for (ms, _) in &tiers {
            assert!(*ms >= MIN_TIMEOUT_MS && *ms <= MAX_TIMEOUT_MS);
        }
    }

    /// A labelled remote with NO timeout env set resolves to the `Default` tier and the
    /// doctor method substitutes `REMOTE_TIMEOUT_MS_DEFAULT` as the effective ms.
    #[test]
    fn resolve_store_sources_timeout_defaults_when_unset() {
        let _g = crate::testenv::lock_env();
        let _v1 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS");
        let _v2 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS_NOENVDB");

        let cfg = Config {
            pull_from: Some(vec!["NOENVDB=libsql://h.turso.io".to_string()]),
            ..Config::default()
        };
        let tiers = cfg.pull_from_remote_timeout_tiers();
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].1, PullTimeoutTier::Default);
        assert_eq!(
            tiers[0].0, REMOTE_TIMEOUT_MS_DEFAULT,
            "default tier reports REMOTE_TIMEOUT_MS_DEFAULT as effective ms"
        );
    }

    /// `pull_from_remote_token_tiers` resolves the per-source / shared / none token
    /// tier for each REMOTE `pull_from` source SYMMETRICALLY to
    /// `peer_db_remote_token_tiers` (same resolver, one label namespace). Locals are
    /// omitted. NEVER carries a token byte (asserted via the redacting Debug).
    #[test]
    fn pull_from_remote_token_tiers_resolves_symmetrically() {
        let _g = crate::testenv::lock_env();
        let var = "WEAVE_PULL_TOKEN_PROD";
        let _v = crate::testenv::EnvVarGuard::set(var, "prod-only-jwt");

        let cfg = Config {
            pull_token: Some("shared-jwt".to_string()),
            pull_from: Some(vec![
                "PROD=libsql://prod.turso.io".to_string(), // per-source label
                "libsql://shared.turso.io".to_string(),    // shared token
                "/tmp/weave-local.db".to_string(),         // local: omitted (no token)
            ]),
            ..Config::default()
        };
        // pull_from view counts one per-source + one shared, omits the local.
        let pull = cfg.pull_from_remote_token_tiers();
        assert_eq!(pull.len(), 2, "locals omitted; two remotes counted");
        assert_eq!(
            pull.iter()
                .filter(|t| **t == PullTokenTier::PerSourceLabel)
                .count(),
            1
        );
        assert_eq!(
            pull.iter().filter(|t| **t == PullTokenTier::Shared).count(),
            1
        );
        assert_eq!(
            pull.iter().filter(|t| **t == PullTokenTier::None).count(),
            0
        );

        // Symmetry: the SAME list as peer_dbs yields the identical tier multiset.
        let cfg_peer = Config {
            peer_dbs: cfg.pull_from.clone(),
            pull_from: None,
            ..cfg.clone()
        };
        let peer = cfg_peer.peer_db_remote_token_tiers();
        assert_eq!(peer, pull, "pull_from and peer_db token tiers must match");

        // No token byte leaks through the tier surface.
        let dbg = format!("{pull:?}");
        assert!(!dbg.contains("prod-only-jwt"), "leaked per-source: {dbg}");
        assert!(!dbg.contains("shared-jwt"), "leaked shared: {dbg}");
    }

    /// `federation_health` aggregates correct symmetric counts/tiers for a configured
    /// mix across BOTH source kinds: a labelled remote (per-source token + per-source
    /// timeout), an unlabelled remote (shared token + global timeout), and a local
    /// file (no token, no remote timeout). `.invalid` hosts only — NO live network
    /// (pure config/env resolution). NEVER carries a token byte.
    #[test]
    fn federation_health_aggregates_mixed_set() {
        let _g = crate::testenv::lock_env();
        let _v1 = crate::testenv::EnvVarGuard::set("WEAVE_PULL_TOKEN_PROD", "prod-only-jwt");
        let _v2 = crate::testenv::EnvVarGuard::set("WEAVE_PULL_TIMEOUT_MS_PROD", "250");
        let _v3 = crate::testenv::EnvVarGuard::set("WEAVE_PULL_TIMEOUT_MS", "1000");

        let mix = vec![
            "PROD=libsql://prod.invalid".to_string(), // per-source token + per-source timeout
            "libsql://shared.invalid".to_string(),    // shared token + global timeout
            "/tmp/weave-fed-health-local.db".to_string(), // local
        ];
        let cfg = Config {
            pull_token: Some("shared-jwt".to_string()),
            peer_dbs: Some(mix.clone()),
            pull_from: Some(mix.clone()),
            ..Config::default()
        };

        let health = cfg.federation_health();
        // Both kinds resolve the SAME list ⇒ identical rollups (parity).
        assert_eq!(
            health.peer_db, health.pull_from,
            "symmetric rollup for an identical source list"
        );

        for kind in [&health.peer_db, &health.pull_from] {
            assert_eq!(kind.total, 3);
            assert_eq!(kind.local, 1);
            assert_eq!(kind.remote, 2);
            // token tiers: one per-source, one shared, none "none".
            assert_eq!(kind.token_per_source, 1);
            assert_eq!(kind.token_shared, 1);
            assert_eq!(kind.token_none, 0);
            // timeout tiers: one per-source (250), one global (1000), none default.
            assert_eq!(kind.timeout_per_source, 1);
            assert_eq!(kind.timeout_global, 1);
            assert_eq!(kind.timeout_default, 0);
            assert_eq!(kind.ms_min, Some(250));
            assert_eq!(kind.ms_max, Some(1000));
        }

        // No token byte leaks through the rollup's Debug.
        let dbg = format!("{health:?}");
        assert!(!dbg.contains("prod-only-jwt"), "leaked per-source: {dbg}");
        assert!(!dbg.contains("shared-jwt"), "leaked shared: {dbg}");
    }

    /// `federation_health` over an empty / local-only config yields zeroed counts and
    /// `None` ms bounds for both kinds — so `doctor` renders no pull-side block and no
    /// misleading `0-0` ms range.
    #[test]
    fn federation_health_empty_and_local_only() {
        let _g = crate::testenv::lock_env();
        let _v = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS");

        // Fully empty.
        let empty = Config::default().federation_health();
        assert_eq!(empty.peer_db, FederationKindHealth::default());
        assert_eq!(empty.pull_from, FederationKindHealth::default());

        // Local-only pull_from: a count but no remote tiers / ms range.
        let cfg = Config {
            pull_from: Some(vec!["/tmp/weave-fed-health-localonly.db".to_string()]),
            ..Config::default()
        };
        let h = cfg.federation_health();
        assert_eq!(h.pull_from.total, 1);
        assert_eq!(h.pull_from.local, 1);
        assert_eq!(h.pull_from.remote, 0);
        assert_eq!(h.pull_from.ms_min, None);
        assert_eq!(h.pull_from.ms_max, None);
        assert_eq!(h.peer_db, FederationKindHealth::default());
    }

    /// `federation_health` classifies the DEFAULT timeout tier (no per-source, no
    /// global env) and reports `REMOTE_TIMEOUT_MS_DEFAULT` as the effective ms bound.
    #[test]
    fn federation_health_default_timeout_tier() {
        let _g = crate::testenv::lock_env();
        let _v1 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS");
        let _v2 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS_DFLT");

        let cfg = Config {
            pull_from: Some(vec!["DFLT=libsql://d.invalid".to_string()]),
            ..Config::default()
        };
        let ph = cfg.federation_health().pull_from;
        assert_eq!(ph.remote, 1);
        assert_eq!(ph.token_none, 1, "no token configured");
        assert_eq!(ph.timeout_default, 1);
        assert_eq!(ph.ms_min, Some(REMOTE_TIMEOUT_MS_DEFAULT));
        assert_eq!(ph.ms_max, Some(REMOTE_TIMEOUT_MS_DEFAULT));
    }

    /// The template documents `WEAVE_PULL_TIMEOUT_MS[_<LABEL>]`, the precedence, the
    /// clamp bounds, and that the LABEL namespace covers BOTH pull_from and peer_dbs.
    #[test]
    fn template_documents_per_source_timeout() {
        assert!(CONFIG_TEMPLATE.contains("WEAVE_PULL_TIMEOUT_MS_<LABEL>"));
        assert!(CONFIG_TEMPLATE.contains("WEAVE_PULL_TIMEOUT_MS"));
        assert!(CONFIG_TEMPLATE.contains("peer_dbs"));
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template parses");
        assert!(cfg.pull_token.is_none());
    }

    // ---- proptest: classify_source totality + resolve ordering/dedup ----

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// `classify_source` is TOTAL: it never panics on arbitrary input (including
        /// unicode, embedded NUL, control chars, scheme-prefix fragments) and its
        /// verdict matches the simple scheme-prefix rule. A Remote verdict preserves
        /// the URL verbatim and attaches no token.
        #[test]
        fn classify_source_is_total(s in ".*") {
            let got = classify_source(&s);
            let expect_remote = REMOTE_SCHEMES.iter().any(|p| s.starts_with(p));
            prop_assert_eq!(got.is_remote(), expect_remote);
            if let StoreSource::Remote { url, token, timeout_ms } = got {
                prop_assert_eq!(url, s);
                prop_assert!(token.is_none());
                prop_assert!(timeout_ms.is_none());
            }
        }

        /// `parse_labeled_source` is TOTAL and upholds its contract on arbitrary
        /// input: a returned label is non-empty, ≤`MAX_LABEL_LEN`, all `[A-Z0-9_]`,
        /// and pairs with a `Remote` source; when no label is returned the result
        /// equals `classify_source` on the verbatim entry (degradation contract).
        #[test]
        fn parse_labeled_source_is_total(s in ".*") {
            let (label, src) = parse_labeled_source(&s);
            match &label {
                Some(l) => {
                    prop_assert!(!l.is_empty());
                    prop_assert!(l.len() <= MAX_LABEL_LEN);
                    prop_assert!(l.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
                    prop_assert!(src.is_remote());
                }
                None => {
                    prop_assert_eq!(&src, &classify_source(&s));
                }
            }
        }

        /// `is_valid_label` is total, and its verdict is invariant under ASCII
        /// uppercasing (case-folding a label never changes its validity).
        #[test]
        fn is_valid_label_is_total_and_case_invariant(s in ".*") {
            let v = is_valid_label(&s);
            prop_assert_eq!(v, is_valid_label(&s.to_ascii_uppercase()));
        }

        /// `parse_clamp_timeout` is TOTAL on arbitrary input: it never panics, and any
        /// `Some(n)` it returns lies within `[MIN_TIMEOUT_MS, MAX_TIMEOUT_MS]` (the
        /// bound is never disabled and never escapes the clamp).
        #[test]
        fn parse_clamp_timeout_is_total_and_bounded(s in ".*") {
            if let Some(n) = parse_clamp_timeout(&s) {
                prop_assert!(n >= MIN_TIMEOUT_MS);
                prop_assert!(n <= MAX_TIMEOUT_MS);
            }
        }

        /// `per_source_timeout` is TOTAL on arbitrary label strings: no panic. (Only
        /// env presence varies the result; here we exercise label totality, not the
        /// env, so the resolved value/tier is not asserted.)
        #[test]
        fn per_source_timeout_is_total_on_arbitrary_label(s in ".*") {
            let _ = per_source_timeout(Some(&s));
        }

        /// `clamp_watch_interval` is TOTAL on ANY `u64` (incl. 0, the boundary
        /// values, and `u64::MAX`): it never panics, always lands inside
        /// `[WATCH_INTERVAL_MIN_SECS, WATCH_INTERVAL_MAX_SECS]`, and is IDEMPOTENT
        /// (clamping an already-clamped value is a no-op). This is the foot-gun
        /// guard for the `--watch --interval` flag — a `0` can never busy-spin the
        /// read loop and a garbage huge value can never freeze the dashboard.
        #[test]
        fn clamp_watch_interval_is_total_and_bounded(secs in any::<u64>()) {
            let c = clamp_watch_interval(secs);
            prop_assert!(c >= WATCH_INTERVAL_MIN_SECS, "below floor: {c}");
            prop_assert!(c <= WATCH_INTERVAL_MAX_SECS, "above ceiling: {c}");
            prop_assert_eq!(clamp_watch_interval(c), c, "clamp must be idempotent");
        }

        /// `resolve_store_sources` is IDEMPOTENT under dedup and stable in order:
        /// feeding the resolved list's own remote URLs back in (in order, twice)
        /// yields the same de-duplicated remote set in the same order. Trailing-slash
        /// variants collapse to a single entry.
        #[test]
        fn resolve_store_sources_remote_dedup_is_stable(
            hosts in proptest::collection::vec("[a-z0-9-]{1,12}", 0..8)
        ) {
            // `pull_from_sources` resolves a per-source timeout from the AMBIENT
            // `WEAVE_PULL_TIMEOUT_MS` (a global env read), so hold the shared env
            // guard and clear that global for this test's duration. Otherwise a
            // concurrent env-mutating test (e.g. `federation_health_*`, which sets
            // `WEAVE_PULL_TIMEOUT_MS`) can change the value BETWEEN the two
            // resolutions below — or mid-resolution across entries — making the
            // `first == second` stability assertion flaky under load. The dedup
            // property under test is independent of the timeout value.
            let _g = crate::testenv::lock_env();
            let _v1 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TIMEOUT_MS");
            let _v2 = crate::testenv::EnvVarGuard::remove("WEAVE_PULL_TOKEN");
            // Build a list with each host twice (plain + trailing slash) to force
            // dedup; first-seen order must be the de-duplicated host order.
            let mut raw: Vec<String> = Vec::new();
            for h in &hosts {
                raw.push(format!("libsql://{h}.turso.io"));
                raw.push(format!("libsql://{h}.turso.io/"));
            }
            let cfg = Config {
                pull_from: Some(raw),
                ..Config::default()
            };
            let first = cfg.pull_from_sources();

            // Distinct hosts, capped — count is min(distinct, MAX_PULL_FROM).
            let mut distinct: Vec<&String> = Vec::new();
            for h in &hosts {
                if !distinct.contains(&h) {
                    distinct.push(h);
                }
            }
            let expected = distinct.len().min(MAX_PULL_FROM);
            prop_assert_eq!(first.len(), expected);

            // Idempotent: re-resolving the resolved URLs (each twice) yields the same.
            let mut raw2: Vec<String> = Vec::new();
            for src in &first {
                if let StoreSource::Remote { url, .. } = src {
                    raw2.push(url.clone());
                    raw2.push(format!("{url}/"));
                }
            }
            let cfg2 = Config { pull_from: Some(raw2), ..Config::default() };
            let second = cfg2.pull_from_sources();
            prop_assert_eq!(first, second);
        }
    }
}
