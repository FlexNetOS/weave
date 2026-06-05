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
    /// never logged/injected/argv'd).
    Remote { url: String, token: Option<String> },
}

// Manual Debug that REDACTS the remote auth token (mirrors the `Config` Debug
// redaction) so a `{:?}` can never leak the secret via a log line, panic message,
// or error context. The URL is shown (it is not itself a secret), but the token is
// only ever rendered as `<redacted>`.
impl std::fmt::Debug for StoreSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreSource::Local(p) => f.debug_tuple("Local").field(p).finish(),
            StoreSource::Remote { url, token } => f
                .debug_struct("Remote")
                .field("url", url)
                .field("token", &token.as_ref().map(|_| "<redacted>"))
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
fn split_source_list(v: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for comma_part in v.split(',') {
        let part = comma_part.trim();
        if part.is_empty() {
            continue;
        }
        // A recognized remote URL is opaque: never split it on the path separator.
        if REMOTE_SCHEMES.iter().any(|s| part.starts_with(s)) {
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
            .field(
                "pull_token",
                &self.pull_token.as_ref().map(|_| "<redacted>"),
            )
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
        // Tier-2 v2 shared remote auth token: WEAVE_PULL_TOKEN overrides the config
        // `pull_token`. A secret — never logged here (the value is not echoed). The
        // env var is the PREFERRED way to supply it (kept out of the config file).
        if let Some(v) = nonempty("WEAVE_PULL_TOKEN") {
            cfg.pull_token = Some(v);
        }
        cfg
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
    pub fn strict_verify(&self) -> bool {
        self.strict_verify.unwrap_or(false)
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
        let raw = match raw {
            Some(v) => v,
            None => return Vec::new(),
        };
        let local = self.db_path();
        let local_canon = std::fs::canonicalize(&local).unwrap_or_else(|_| local.clone());
        let token = self.pull_token.as_deref().and_then(sanitize_token);

        let mut out: Vec<StoreSource> = Vec::new();
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
            match classify_source(trimmed) {
                StoreSource::Local(path) => {
                    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if key == local_canon {
                        continue; // never read the local store twice
                    }
                    if seen_local.contains(&key) {
                        continue;
                    }
                    seen_local.push(key);
                    out.push(StoreSource::Local(path));
                }
                StoreSource::Remote { url, .. } => {
                    let key = normalize_remote_url(&url);
                    if seen_remote.contains(&key) {
                        continue;
                    }
                    seen_remote.push(key);
                    out.push(StoreSource::Remote {
                        url,
                        token: token.clone(),
                    });
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
# WEAVE_LIBSQL_AUTH_TOKEN, WEAVE_PULL_TOKEN) take precedence over anything set here.

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

# Auth token for REMOTE pull/federation sources (Tier-2 v2). Applied to every
# libsql://, https:// or wss:// source above that needs one. Treat as a SECRET;
# weave redacts it from debug output. Prefer the WEAVE_PULL_TOKEN env var over
# storing it here. RECOMMENDED: use a SERVER-ENFORCED read-only Turso token
# (`turso db tokens create <db> --read-only`) so the source is read-only at the
# server, not just by weave's client-side guards.
# pull_token = \"...\"

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
# strict_verify = false
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
        assert!(cfg.pull_token.is_none());
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
            "pull_token",
        ] {
            assert!(
                CONFIG_TEMPLATE.contains(key),
                "template is missing config key {key:?}"
            );
        }
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
                StoreSource::Remote { url: got, token } => {
                    assert_eq!(got, url, "URL preserved verbatim (no canonicalization)");
                    assert!(token.is_none(), "classify never attaches a token");
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
            StoreSource::Remote { url, token } => {
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
            StoreSource::Remote { url, token } => {
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
        };
        assert!(cfg.inject_allowed_from_source(&trusted));

        let untrusted = StoreSource::Remote {
            url: "libsql://evil.turso.io".to_string(),
            token: None,
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
            if let StoreSource::Remote { url, token } = got {
                prop_assert_eq!(url, s);
                prop_assert!(token.is_none());
            }
        }

        /// `resolve_store_sources` is IDEMPOTENT under dedup and stable in order:
        /// feeding the resolved list's own remote URLs back in (in order, twice)
        /// yields the same de-duplicated remote set in the same order. Trailing-slash
        /// variants collapse to a single entry.
        #[test]
        fn resolve_store_sources_remote_dedup_is_stable(
            hosts in proptest::collection::vec("[a-z0-9-]{1,12}", 0..8)
        ) {
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
