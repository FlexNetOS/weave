//! CC Switch provider-store bridge.
//!
//! This is intentionally a small Rust-native bridge instead of embedding the
//! Tauri application. It consumes CC Switch's stable on-disk provider store
//! (`~/.cc-switch/cc-switch.db`) and applies selected provider snapshots to the
//! host config files while preserving weave-owned lifecycle hooks.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ProviderSwitchApp {
    Claude,
    Codex,
    Gemini,
}

impl ProviderSwitchApp {
    pub(crate) fn as_cc_switch(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    fn display(self) -> &'static str {
        self.as_cc_switch()
    }
}

#[derive(Debug)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub is_current: bool,
    pub settings_config: Value,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProviderMeta {
    common_config_enabled: Option<bool>,
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn default_db_path() -> PathBuf {
    home().join(".cc-switch").join("cc-switch.db")
}

fn claude_settings_path() -> PathBuf {
    home().join(".claude").join("settings.json")
}

fn codex_config_path() -> PathBuf {
    home().join(".codex").join("config.toml")
}

fn codex_auth_path() -> PathBuf {
    home().join(".codex").join("auth.json")
}

fn gemini_env_path() -> PathBuf {
    home().join(".gemini").join(".env")
}

fn gemini_settings_path() -> PathBuf {
    home().join(".gemini").join("settings.json")
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing JSON {}", path.display()))
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.weave-provider-switch.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("opening temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_private(path, &bytes)
}

fn read_text_if_exists(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn connect(db_path: Option<PathBuf>) -> Result<Connection> {
    let path = db_path.unwrap_or_else(default_db_path);
    Connection::open(&path).with_context(|| format!("opening CC Switch DB {}", path.display()))
}

pub fn list(db_path: Option<PathBuf>, app: ProviderSwitchApp) -> Result<Vec<ProviderRow>> {
    let conn = connect(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, settings_config, category, is_current FROM providers \
         WHERE app_type = ?1 ORDER BY is_current DESC, COALESCE(sort_index, 999999), name, id",
    )?;
    let rows = stmt
        .query_map(params![app.as_cc_switch()], |row| {
            let settings_text: String = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                settings_text,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(|(id, name, settings_text, category, is_current)| {
            let settings_config: Value = serde_json::from_str(&settings_text)
                .with_context(|| format!("provider {id} has invalid settings_config JSON"))?;
            Ok(ProviderRow {
                id,
                name,
                category,
                is_current,
                settings_config,
            })
        })
        .collect()
}

pub fn current(db_path: Option<PathBuf>, app: ProviderSwitchApp) -> Result<Option<ProviderRow>> {
    Ok(list(db_path, app)?.into_iter().find(|p| p.is_current))
}

pub fn switch(
    db_path: Option<PathBuf>,
    app: ProviderSwitchApp,
    provider_id: &str,
    dry_run: bool,
) -> Result<ProviderRow> {
    let path = db_path.unwrap_or_else(default_db_path);
    let conn = Connection::open(&path)
        .with_context(|| format!("opening CC Switch DB {}", path.display()))?;
    let app_name = app.as_cc_switch();
    let mut provider = load_provider(&conn, app, provider_id)?;

    let common = load_common_config(&conn, app_name)?;
    provider.settings_config =
        apply_common_config_if_enabled(&provider.settings_config, common.as_deref())?;

    if dry_run {
        return Ok(provider);
    }

    apply_live(app, &provider)?;

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE providers SET is_current = 0 WHERE app_type = ?1",
        params![app_name],
    )?;
    let changed = tx.execute(
        "UPDATE providers SET is_current = 1 WHERE id = ?1 AND app_type = ?2",
        params![provider_id, app_name],
    )?;
    if changed != 1 {
        bail!("provider {provider_id} disappeared while switching {app_name}");
    }
    tx.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![format!("current_provider_{app_name}"), provider_id],
    )?;
    tx.commit()?;

    Ok(provider)
}

fn load_provider(
    conn: &Connection,
    app: ProviderSwitchApp,
    provider_id: &str,
) -> Result<ProviderRow> {
    let row = conn
        .query_row(
            "SELECT id, name, settings_config, category, is_current FROM providers \
             WHERE id = ?1 AND app_type = ?2",
            params![provider_id, app.as_cc_switch()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .optional()?
        .with_context(|| {
            format!(
                "no {} provider with id `{provider_id}` in CC Switch DB",
                app.display()
            )
        })?;
    let settings_config = serde_json::from_str(&row.2)
        .with_context(|| format!("provider {provider_id} has invalid settings_config JSON"))?;
    Ok(ProviderRow {
        id: row.0,
        name: row.1,
        settings_config,
        category: row.3,
        is_current: row.4,
    })
}

fn load_common_config(conn: &Connection, app: &str) -> Result<Option<String>> {
    let key = format!("common_config_{app}");
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn provider_meta(settings: &Value) -> ProviderMeta {
    settings
        .get("meta")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn apply_common_config_if_enabled(settings: &Value, common: Option<&str>) -> Result<Value> {
    let Some(snippet) = common.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(settings.clone());
    };
    if provider_meta(settings).common_config_enabled != Some(true) {
        return Ok(settings.clone());
    }
    let mut out = settings.clone();
    // CC Switch common config is JSON for Claude/Gemini env-ish settings and TOML
    // for Codex. For this bridge, only JSON snippets are merged generically; Codex
    // providers normally carry their complete config TOML in settings_config.config.
    if let Ok(Value::Object(source)) = serde_json::from_str::<Value>(snippet) {
        if let Some(target) = out.as_object_mut() {
            for (k, v) in source {
                target.insert(k, v);
            }
        }
    }
    Ok(out)
}

fn apply_live(app: ProviderSwitchApp, provider: &ProviderRow) -> Result<()> {
    match app {
        ProviderSwitchApp::Claude => apply_claude(provider),
        ProviderSwitchApp::Codex => apply_codex(provider),
        ProviderSwitchApp::Gemini => apply_gemini(provider),
    }
}

fn apply_claude(provider: &ProviderRow) -> Result<()> {
    let path = claude_settings_path();
    let mut next = provider.settings_config.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.remove("api_format");
        obj.remove("apiFormat");
        obj.remove("openrouter_compat_mode");
        obj.remove("openrouterCompatMode");
    }

    // Preserve hook/MCP blocks already owned by weave or another tool; CC Switch's
    // native switch overwrites settings.json, but in weave the lifecycle wiring is
    // part of the product contract and must survive provider switching.
    if let Some(existing) = read_text_if_exists(&path)?.and_then(|_| read_json(&path).ok()) {
        for key in ["hooks", "mcpServers"] {
            if next.get(key).is_none() {
                if let Some(v) = existing.get(key) {
                    next.as_object_mut()
                        .context("Claude provider settings must be a JSON object")?
                        .insert(key.to_string(), v.clone());
                }
            }
        }
    }
    write_json(&path, &next)?;
    Ok(())
}

fn apply_codex(provider: &ProviderRow) -> Result<()> {
    let obj = provider
        .settings_config
        .as_object()
        .context("Codex provider settings must be a JSON object")?;
    let config_text = obj
        .get("config")
        .and_then(Value::as_str)
        .context("Codex provider settings missing string `config`")?;

    // Preserve weave's notify hook line if it is already installed.
    let existing = read_text_if_exists(&codex_config_path())?.unwrap_or_default();
    let notify = existing
        .lines()
        .find(|line| {
            line.trim_start().starts_with("notify =")
                && line.contains("hook")
                && line.contains("wake")
        })
        .map(str::to_string);
    let mut out = config_text.trim_end().to_string();
    out.push('\n');
    if let Some(notify) = notify {
        if !out
            .lines()
            .any(|line| line.trim_start().starts_with("notify ="))
        {
            out.push('\n');
            out.push_str(&notify);
            out.push('\n');
        }
    }
    atomic_write_private(&codex_config_path(), out.as_bytes())?;

    if let Some(auth) = obj.get("auth") {
        // Write only explicit API-key style auth; avoid clobbering OAuth/session
        // material with an empty object from a provider preset.
        if auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            write_json(&codex_auth_path(), auth)?;
        }
    }
    Ok(())
}

fn apply_gemini(provider: &ProviderRow) -> Result<()> {
    let obj = provider
        .settings_config
        .as_object()
        .context("Gemini provider settings must be a JSON object")?;
    if let Some(env) = obj.get("env").and_then(Value::as_object) {
        let mut lines = Vec::new();
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                lines.push(format!("{k}={}", shell_env_quote(s)));
            }
        }
        lines.sort();
        let mut text = lines.join("\n");
        text.push('\n');
        atomic_write_private(&gemini_env_path(), text.as_bytes())?;
    }

    if let Some(config) = obj.get("config").filter(|v| v.is_object()) {
        let path = gemini_settings_path();
        let mut merged = read_text_if_exists(&path)?
            .and_then(|_| read_json(&path).ok())
            .unwrap_or_else(|| json!({}));
        let merged_obj = merged
            .as_object_mut()
            .context("existing Gemini settings must be a JSON object")?;
        for (k, v) in config.as_object().unwrap() {
            merged_obj.insert(k.clone(), v.clone());
        }
        write_json(&path, &merged)?;
    }
    Ok(())
}

fn shell_env_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
    {
        return s.to_string();
    }
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
