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

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    pub session: Option<String>,
    pub backend: Option<String>,
    pub db: Option<String>,
    pub nudge_template: Option<String>,
    pub libsql_url: Option<String>,
    pub libsql_auth_token: Option<String>,
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
        cfg
    }

    pub fn backend(&self) -> String {
        self.backend.clone().unwrap_or_else(|| "sqlite".to_string())
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
