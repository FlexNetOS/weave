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
            .finish()
    }
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
        .join("weave")
        .join("config.toml")
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
        cfg
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
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join(".local/share"))
            .join("weave")
            .join("messages.db")
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
# WEAVE_LIBSQL_AUTH_TOKEN) take precedence over anything set here.

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
}
