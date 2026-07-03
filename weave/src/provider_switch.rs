//! CC Switch provider-store bridge.
//!
//! This is intentionally a small Rust-native bridge instead of embedding the
//! Tauri application. It consumes CC Switch's stable on-disk provider store
//! (`~/.cc-switch/cc-switch.db`) and applies selected provider snapshots to the
//! host config files while preserving weave-owned lifecycle hooks.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    pub app: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub source: String,
    pub current: bool,
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
    list_from_conn(&conn, app)
}

fn list_from_conn(conn: &Connection, app: ProviderSwitchApp) -> Result<Vec<ProviderRow>> {
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

pub fn models(
    db_path: Option<PathBuf>,
    app: ProviderSwitchApp,
    include_ollama: bool,
) -> Result<Vec<ModelRow>> {
    let conn = connect(db_path)?;
    let mut out = Vec::new();
    for provider in list_from_conn(&conn, app)? {
        for (model, source) in provider_models(app, &provider) {
            out.push(ModelRow {
                app: app.as_cc_switch().to_string(),
                provider_id: provider.id.clone(),
                provider_name: provider.name.clone(),
                current: provider.is_current && model == current_model(app, &provider),
                model,
                source,
            });
        }
    }

    if include_ollama {
        match ollama_models() {
            Ok(models) => {
                for model in models {
                    out.push(ModelRow {
                        app: app.as_cc_switch().to_string(),
                        provider_id: "ollama-local".to_string(),
                        provider_name: "Ollama Local".to_string(),
                        model,
                        source: "ollama".to_string(),
                        current: false,
                    });
                }
            }
            Err(err) => eprintln!("[weave] provider-switch: Ollama model probe skipped: {err}"),
        }
    }

    out.sort_by(|a, b| {
        a.provider_id
            .cmp(&b.provider_id)
            .then(a.source.cmp(&b.source))
            .then(a.model.cmp(&b.model))
    });
    out.dedup_by(|a, b| {
        a.provider_id == b.provider_id && a.source == b.source && a.model == b.model
    });
    Ok(out)
}

/// Secret-free CC Switch bridge diagnostics for orchestration/status surfaces.
///
/// This intentionally opens the DB read-only and never applies provider config:
/// absent or unreadable stores are diagnostic states, not hard failures. The
/// bridge only owns visibility into the CC Switch store; CC Switch remains the
/// source of truth for provider/proxy/failover semantics.
pub fn status(db_path: Option<PathBuf>) -> Value {
    let path = db_path.unwrap_or_else(default_db_path);
    let path_text = path.to_string_lossy().to_string();
    let supported_apps = [
        ProviderSwitchApp::Claude,
        ProviderSwitchApp::Codex,
        ProviderSwitchApp::Gemini,
    ];
    let app_coverage = json!({
        "claude": {"supported": true},
        "claude-desktop": {"supported": false},
        "codex": {"supported": true},
        "gemini": {"supported": true},
        "opencode": {"supported": false},
        "openclaw": {"supported": false},
        "hermes": {"supported": false},
    });

    if !path.exists() {
        return json!({
            "db_path": path_text,
            "db_present": false,
            "db_readable": false,
            "schema_ok": false,
            "error": "cc-switch-db-missing",
            "supported_apps": supported_apps.iter().map(|a| a.as_cc_switch()).collect::<Vec<_>>(),
            "app_coverage": app_coverage,
        });
    }

    let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(err) => {
            return json!({
                "db_path": path_text,
                "db_present": true,
                "db_readable": false,
                "schema_ok": false,
                "error": format!("opening CC Switch DB read-only: {err}"),
                "supported_apps": supported_apps.iter().map(|a| a.as_cc_switch()).collect::<Vec<_>>(),
                "app_coverage": app_coverage,
            });
        }
    };

    let providers_exists = table_exists(&conn, "providers").unwrap_or(false);
    let settings_exists = table_exists(&conn, "settings").unwrap_or(false);
    let provider_health_exists = table_exists(&conn, "provider_health").unwrap_or(false);
    let proxy_config_exists = table_exists(&conn, "proxy_config").unwrap_or(false);
    let failover_queue_exists = table_exists(&conn, "failover_queue").unwrap_or(false);
    let usage_logs_exists = table_exists(&conn, "usage_logs").unwrap_or(false)
        || table_exists(&conn, "proxy_usage_logs").unwrap_or(false);
    let prompt_sync_exists = table_exists(&conn, "prompt_sync").unwrap_or(false)
        || table_exists(&conn, "prompts").unwrap_or(false);
    let skill_sync_exists = table_exists(&conn, "skill_sync").unwrap_or(false)
        || table_exists(&conn, "skills").unwrap_or(false);
    let mcp_sync_exists = table_exists(&conn, "mcp_sync").unwrap_or(false)
        || table_exists(&conn, "mcp_servers").unwrap_or(false);

    let provider_columns = if providers_exists {
        column_names(&conn, "providers").unwrap_or_default()
    } else {
        Vec::new()
    };
    let required = ["id", "app_type", "name", "settings_config", "is_current"];
    let missing_columns = required
        .iter()
        .filter(|col| !provider_columns.iter().any(|have| have == **col))
        .copied()
        .collect::<Vec<_>>();
    let schema_ok = providers_exists && missing_columns.is_empty();

    let apps = supported_apps
        .iter()
        .map(|app| provider_status_for_app(&conn, *app, schema_ok))
        .collect::<Vec<_>>();
    let app_types = if schema_ok {
        distinct_strings(
            &conn,
            "SELECT DISTINCT app_type FROM providers ORDER BY app_type",
        )
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    json!({
        "db_path": path_text,
        "db_present": true,
        "db_readable": true,
        "schema_ok": schema_ok,
        "missing_provider_columns": missing_columns,
        "tables": {
            "providers": providers_exists,
            "settings": settings_exists,
            "provider_health": provider_health_exists,
            "proxy_config": proxy_config_exists,
            "failover_queue": failover_queue_exists,
            "usage_logs": usage_logs_exists,
            "mcp_sync": mcp_sync_exists,
            "prompt_sync": prompt_sync_exists,
            "skill_sync": skill_sync_exists,
        },
        "supported_apps": supported_apps.iter().map(|a| a.as_cc_switch()).collect::<Vec<_>>(),
        "known_app_types": app_types,
        "app_coverage": app_coverage,
        "apps": apps,
        "proxy_health": {
            "proxy_config_present": proxy_config_exists,
            "failover_queue_present": failover_queue_exists,
            "provider_health_present": provider_health_exists,
            "usage_logs_present": usage_logs_exists,
        }
    })
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn distinct_strings(conn: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn count_for(conn: &Connection, sql: &str, param: &str) -> Option<i64> {
    conn.query_row(sql, params![param], |row| row.get::<_, i64>(0))
        .ok()
}

fn provider_status_for_app(conn: &Connection, app: ProviderSwitchApp, schema_ok: bool) -> Value {
    let app_name = app.as_cc_switch();
    if !schema_ok {
        return json!({
            "app": app_name,
            "supported": true,
            "providers": 0,
            "current_provider": null,
            "current_model": null,
            "settings_current_provider": null,
            "live_config_present": live_config_present(app),
            "live_config_agrees": null,
        });
    }

    let providers = count_for(
        conn,
        "SELECT COUNT(*) FROM providers WHERE app_type = ?1",
        app_name,
    )
    .unwrap_or(0);
    let current = current_from_conn(conn, app).ok().flatten();
    let settings_current = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![format!("current_provider_{app_name}")],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten();
    let (current_provider, current_model_value) = current
        .as_ref()
        .map(|row| {
            (
                Some(json!({"id": row.id, "name": row.name, "category": row.category})),
                current_model_opt(app, row)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            )
        })
        .unwrap_or((None, Value::Null));
    let live_present = live_config_present(app);
    let live_agrees = current
        .as_ref()
        .and_then(|row| live_config_agrees(app, row).ok());

    json!({
        "app": app_name,
        "supported": true,
        "providers": providers,
        "current_provider": current_provider,
        "current_model": current_model_value,
        "settings_current_provider": settings_current,
        "live_config_present": live_present,
        "live_config_agrees": live_agrees,
    })
}

fn current_from_conn(conn: &Connection, app: ProviderSwitchApp) -> Result<Option<ProviderRow>> {
    Ok(list_from_conn(conn, app)?
        .into_iter()
        .find(|p| p.is_current))
}

fn live_config_present(app: ProviderSwitchApp) -> bool {
    match app {
        ProviderSwitchApp::Claude => claude_settings_path().exists(),
        ProviderSwitchApp::Codex => codex_config_path().exists(),
        ProviderSwitchApp::Gemini => gemini_env_path().exists() || gemini_settings_path().exists(),
    }
}

fn live_config_agrees(app: ProviderSwitchApp, provider: &ProviderRow) -> Result<Option<bool>> {
    let Some(expected_model) = current_model_opt(app, provider) else {
        return Ok(None);
    };
    match app {
        ProviderSwitchApp::Claude => {
            let path = claude_settings_path();
            if !path.exists() {
                return Ok(Some(false));
            }
            let live = read_json(&path)?;
            Ok(Some(
                live.get("model").and_then(Value::as_str).map(str::trim)
                    == Some(expected_model.as_str()),
            ))
        }
        ProviderSwitchApp::Codex => {
            let Some(text) = read_text_if_exists(&codex_config_path())? else {
                return Ok(Some(false));
            };
            Ok(Some(
                extract_codex_model(&text).as_deref() == Some(expected_model.as_str()),
            ))
        }
        ProviderSwitchApp::Gemini => {
            if let Some(text) = read_text_if_exists(&gemini_env_path())? {
                let agrees = text.lines().any(|line| {
                    let trimmed = line.trim();
                    trimmed == format!("GEMINI_MODEL={expected_model}")
                        || trimmed == format!("GOOGLE_GEMINI_MODEL={expected_model}")
                });
                return Ok(Some(agrees));
            }
            let path = gemini_settings_path();
            if !path.exists() {
                return Ok(Some(false));
            }
            let live = read_json(&path)?;
            Ok(Some(
                live.pointer("/env/GEMINI_MODEL")
                    .or_else(|| live.pointer("/env/GOOGLE_GEMINI_MODEL"))
                    .or_else(|| live.pointer("/model"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    == Some(expected_model.as_str()),
            ))
        }
    }
}

pub fn switch_model(
    db_path: Option<PathBuf>,
    app: ProviderSwitchApp,
    provider_id: &str,
    model: &str,
    dry_run: bool,
) -> Result<ProviderRow> {
    let path = db_path.unwrap_or_else(default_db_path);
    let conn = Connection::open(&path)
        .with_context(|| format!("opening CC Switch DB {}", path.display()))?;
    let app_name = app.as_cc_switch();
    let mut provider = load_provider(&conn, app, provider_id)?;
    set_model(app, &mut provider.settings_config, model)
        .with_context(|| format!("setting {app_name} model for provider `{provider_id}`"))?;

    if dry_run {
        return Ok(provider);
    }

    let settings_text = serde_json::to_string(&provider.settings_config)?;
    let changed = conn.execute(
        "UPDATE providers SET settings_config = ?1 WHERE id = ?2 AND app_type = ?3",
        params![settings_text, provider_id, app_name],
    )?;
    if changed != 1 {
        bail!("provider {provider_id} disappeared while switching model for {app_name}");
    }
    if provider.is_current {
        apply_live(app, &provider)?;
    }
    Ok(provider)
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

fn provider_models(app: ProviderSwitchApp, provider: &ProviderRow) -> Vec<(String, String)> {
    let mut out = BTreeSet::<(String, String)>::new();
    if let Some(model) = current_model_opt(app, provider) {
        out.insert((model, "current".to_string()));
    }
    collect_catalog_models(&provider.settings_config, &mut out);
    out.into_iter().collect()
}

fn current_model(app: ProviderSwitchApp, provider: &ProviderRow) -> String {
    current_model_opt(app, provider).unwrap_or_default()
}

fn current_model_opt(app: ProviderSwitchApp, provider: &ProviderRow) -> Option<String> {
    match app {
        ProviderSwitchApp::Claude => provider
            .settings_config
            .get("model")
            .or_else(|| provider.settings_config.pointer("/env/ANTHROPIC_MODEL"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        ProviderSwitchApp::Codex => provider
            .settings_config
            .get("config")
            .and_then(Value::as_str)
            .and_then(extract_codex_model),
        ProviderSwitchApp::Gemini => provider
            .settings_config
            .pointer("/env/GEMINI_MODEL")
            .or_else(|| provider.settings_config.pointer("/env/GOOGLE_GEMINI_MODEL"))
            .or_else(|| provider.settings_config.pointer("/config/model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

fn set_model(app: ProviderSwitchApp, settings: &mut Value, model: &str) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        bail!("model must not be empty");
    }
    let obj = settings
        .as_object_mut()
        .with_context(|| format!("{} provider settings must be a JSON object", app.display()))?;
    match app {
        ProviderSwitchApp::Claude => {
            obj.insert("model".to_string(), Value::String(model.to_string()));
        }
        ProviderSwitchApp::Codex => {
            let config = obj
                .get("config")
                .and_then(Value::as_str)
                .context("Codex provider settings missing string `config`")?;
            obj.insert(
                "config".to_string(),
                Value::String(set_codex_model(config, model)),
            );
        }
        ProviderSwitchApp::Gemini => {
            let env = obj.entry("env").or_insert_with(|| json!({}));
            let env_obj = env
                .as_object_mut()
                .context("Gemini provider settings `env` must be an object")?;
            env_obj.insert("GEMINI_MODEL".to_string(), Value::String(model.to_string()));
        }
    }
    Ok(())
}

fn collect_catalog_models(value: &Value, out: &mut BTreeSet<(String, String)>) {
    fn visit(value: &Value, source: &str, out: &mut BTreeSet<(String, String)>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let next_source = if key.eq_ignore_ascii_case("modelCatalog")
                        || key.eq_ignore_ascii_case("models")
                    {
                        key.as_str()
                    } else {
                        source
                    };
                    if matches!(key.as_str(), "model" | "id" | "name") {
                        if let Some(model) = value.as_str().map(str::trim).filter(|s| !s.is_empty())
                        {
                            out.insert((model.to_string(), next_source.to_string()));
                        }
                    }
                    visit(value, next_source, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    if let Some(model) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        out.insert((model.to_string(), source.to_string()));
                    } else {
                        visit(item, source, out);
                    }
                }
            }
            Value::String(model) if source != "settings" => {
                let model = model.trim();
                if !model.is_empty() && !model.contains('\n') && model.len() < 128 {
                    out.insert((model.to_string(), source.to_string()));
                }
            }
            _ => {}
        }
    }
    if let Some(catalog) = value.get("modelCatalog") {
        visit(catalog, "modelCatalog", out);
    }
    if let Some(models) = value.get("models") {
        visit(models, "models", out);
    }
}

fn extract_codex_model(config: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with('[') || !trimmed.starts_with("model") {
            return None;
        }
        let (key, value) = trimmed.split_once('=')?;
        if key.trim() != "model" {
            return None;
        }
        Some(unquote_toml_string(value.trim()))
    })
}

fn set_codex_model(config: &str, model: &str) -> String {
    let replacement = format!("model = {}", toml_quote(model));
    let mut replaced = false;
    let mut out = Vec::new();
    for line in config.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            if let Some((key, _)) = trimmed.split_once('=') {
                if key.trim() == "model" {
                    out.push(replacement.clone());
                    replaced = true;
                    continue;
                }
            }
        }
        if !replaced && trimmed.starts_with('[') {
            out.push(replacement.clone());
            replaced = true;
        }
        out.push(line.to_string());
    }
    if !replaced {
        out.push(replacement);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn unquote_toml_string(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        value.to_string()
    }
}

fn toml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn ollama_models() -> Result<Vec<String>> {
    let host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let (host_header, addr, path) = parse_http_url(&host, "/api/tags")?;
    let mut stream =
        TcpStream::connect(&addr).with_context(|| format!("connecting to Ollama at {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    )?;
    let mut response = String::new();
    if let Err(err) = stream.read_to_string(&mut response) {
        if response.is_empty() {
            return Err(err.into());
        }
    }
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("invalid Ollama HTTP response")?;
    let body = if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("Transfer-Encoding: chunked"))
    {
        decode_http_chunked(body).context("decoding Ollama chunked response")?
    } else {
        body.to_string()
    };
    let json: Value = serde_json::from_str(&body).context("parsing Ollama /api/tags response")?;
    let mut out = Vec::new();
    if let Some(models) = json.get("models").and_then(Value::as_array) {
        for item in models {
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                if !name.trim().is_empty() {
                    out.push(name.trim().to_string());
                }
            }
        }
    }
    Ok(out)
}

fn decode_http_chunked(mut body: &str) -> Result<String> {
    let mut out = String::new();
    loop {
        let (len_hex, rest) = body
            .split_once("\r\n")
            .context("chunk missing length terminator")?;
        let len_hex = len_hex.split(';').next().unwrap_or(len_hex).trim();
        let len = usize::from_str_radix(len_hex, 16)
            .with_context(|| format!("invalid chunk length `{len_hex}`"))?;
        body = rest;
        if len == 0 {
            break;
        }
        if body.len() < len + 2 {
            bail!("chunk body shorter than declared length");
        }
        out.push_str(&body[..len]);
        body = &body[len..];
        body = body
            .strip_prefix("\r\n")
            .context("chunk missing trailing CRLF")?;
    }
    Ok(out)
}

fn parse_http_url(base: &str, default_path: &str) -> Result<(String, String, String)> {
    let rest = base
        .strip_prefix("http://")
        .context("OLLAMA_HOST must be an http:// URL for this local smoke")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, suffix)) => (authority, format!("/{suffix}")),
        None => (rest, default_path.to_string()),
    };
    let authority = authority.trim();
    if authority.is_empty() {
        bail!("OLLAMA_HOST has an empty host");
    }
    let addr = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((authority.to_string(), addr, path))
}

fn merge_json_objects(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, source_value) in source_map {
                match target_map.get_mut(key) {
                    Some(target_value) => merge_json_objects(target_value, source_value),
                    None => {
                        target_map.insert(key.clone(), source_value.clone());
                    }
                }
            }
        }
        (target_value, source_value) => {
            *target_value = source_value.clone();
        }
    }
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
        merged
            .as_object_mut()
            .context("existing Gemini settings must be a JSON object")?;
        merge_json_objects(&mut merged, config);
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
