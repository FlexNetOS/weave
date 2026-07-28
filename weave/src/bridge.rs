//! Shared, bounded bridge delivery primitives for Telegram and Slack.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use weave_core::config::{BridgeConfigView, Config};
use weave_core::model::{
    BridgeCursorEnvelope, BridgePlatform, BridgeRuntimeState, BridgeRuntimeStatus, DeliveryOutcome,
    DeliveryRefKind, DeliveryStage, Message,
};
use weave_core::store::Store;

pub const MAX_BRIDGE_RESPONSE_BYTES: usize = 1_048_576;
pub const MAX_BRIDGE_POST_RESPONSE_BYTES: usize = 65_536;
pub const MAX_BRIDGE_TEXT_CHARS: usize = 4_000;
pub const MAX_BRIDGE_BATCH: usize = 50;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RelayReport {
    pub attempted: usize,
    pub delivered: usize,
    pub pending: bool,
    pub error_class: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCallError {
    pub class: String,
    pub message: String,
}

/// Decode only the cursor schema this binary understands. Serde normally ignores
/// unknown object fields, which is useful for most wire formats but unsafe for a
/// durable progress token: silently accepting a future schema could move a
/// watermark under semantics this process does not understand.
pub fn decode_cursor_strict(
    encoded: &str,
) -> std::result::Result<Option<BridgeCursorEnvelope>, BridgeCallError> {
    let decoded = BridgeCursorEnvelope::decode(encoded)
        .map_err(|_| BridgeCallError::new("cursor", "bridge cursor state is invalid"))?;
    if decoded.is_none() {
        return Ok(None);
    }
    let value = serde_json::from_str::<Value>(encoded)
        .map_err(|_| BridgeCallError::new("cursor", "bridge cursor state is invalid"))?;
    let object = value
        .as_object()
        .ok_or_else(|| BridgeCallError::new("cursor", "bridge cursor state is invalid"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "external_identity" | "external_scope" | "position" | "continuation"
        )
    }) {
        return Err(BridgeCallError::new(
            "cursor",
            "bridge cursor schema is not supported",
        ));
    }
    Ok(decoded)
}

fn cursor_scope_matches(platform: BridgePlatform, configured: Option<&str>, actual: &str) -> bool {
    match (platform, configured) {
        // Telegram resolves @username labels to a numeric chat id before binding
        // the durable cursor and provider sends. Any numeric scope here is the
        // checked canonical route corresponding to the configured label.
        (BridgePlatform::Telegram, Some(label)) if label.starts_with('@') => {
            actual.parse::<i128>().is_ok()
        }
        (_, Some(expected)) => expected == actual,
        (_, None) => false,
    }
}

/// Token-free result of a bounded, non-consuming platform identity check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeCheck {
    pub platform: BridgePlatform,
    pub external_identity: Option<String>,
    pub external_scope: Option<String>,
}

/// Combined token-free configuration, durable runtime, and pending-mail view.
#[derive(Debug, Clone, Serialize)]
pub struct BridgeStatusSnapshot {
    pub config: BridgeConfigView,
    pub runtime: Option<BridgeRuntimeState>,
    pub pending_outbound: i64,
    pub active: bool,
    pub stale: bool,
    pub healthy: bool,
    pub status: String,
}

pub fn bridge_status_snapshot(
    store: &dyn Store,
    config: &Config,
    platform: BridgePlatform,
) -> Result<BridgeStatusSnapshot> {
    bridge_status_snapshot_at(store, config, platform, weave_core::model::now())
}

fn bridge_status_snapshot_at(
    store: &dyn Store,
    config: &Config,
    platform: BridgePlatform,
    observed_at: i64,
) -> Result<BridgeStatusSnapshot> {
    let view = match platform {
        BridgePlatform::Telegram => config.telegram_bridge_config_view(),
        BridgePlatform::Slack => config.slack_bridge_config_view(),
    };
    let pending_outbound = match view.identity.as_deref() {
        Some(identity) => store.unread_count(identity)?,
        None => 0,
    };
    let runtime = store.bridge_runtime_status(platform)?;
    let active = runtime
        .as_ref()
        .is_some_and(|state| state.is_active_at(observed_at));
    let stale = runtime
        .as_ref()
        .is_some_and(|state| state.is_stale_at(observed_at));
    let healthy = view.ready
        && active
        && runtime.as_ref().is_some_and(|state| {
            let external_route_matches = decode_cursor_strict(&state.cursor)
                .ok()
                .flatten()
                .is_some_and(|cursor| {
                    cursor_scope_matches(
                        platform,
                        view.conversation.as_deref(),
                        &cursor.external_scope,
                    )
                });
            state.status == BridgeRuntimeStatus::Running
                && state.last_error_class.is_empty()
                && view.identity.as_deref() == Some(state.identity.as_str())
                && view.recipient.as_deref() == Some(state.recipient.as_str())
                && external_route_matches
        });
    let status = bridge_status_token_for(&view, active, stale, healthy).to_string();
    Ok(BridgeStatusSnapshot {
        config: view,
        runtime,
        pending_outbound,
        active,
        stale,
        healthy,
        status,
    })
}

pub fn bridge_statuses_json(store: &dyn Store, config: &Config) -> Result<Value> {
    let telegram = bridge_status_snapshot(store, config, BridgePlatform::Telegram)?;
    let slack = bridge_status_snapshot(store, config, BridgePlatform::Slack)?;
    Ok(serde_json::json!({"telegram": telegram, "slack": slack}))
}

fn bridge_status_token_for(
    config: &BridgeConfigView,
    active: bool,
    stale: bool,
    healthy: bool,
) -> &'static str {
    if healthy {
        "healthy"
    } else if active {
        "degraded"
    } else if stale {
        "stale"
    } else if config.ready {
        "ready_inactive"
    } else if config.configured {
        "not_ready"
    } else {
        "not_configured"
    }
}

pub fn bridge_status_token(snapshot: &BridgeStatusSnapshot) -> &str {
    &snapshot.status
}

pub fn bridge_status_line(snapshot: &BridgeStatusSnapshot) -> String {
    let identity = snapshot.config.identity.as_deref().unwrap_or("-");
    let recipient = snapshot.config.recipient.as_deref().unwrap_or("-");
    let mut line = format!(
        "{}: status={} identity={} recipient={} pending={}",
        snapshot.config.platform.as_str(),
        bridge_status_token(snapshot),
        identity,
        recipient,
        snapshot.pending_outbound
    );
    if !snapshot.config.issues.is_empty() {
        let issues = snapshot
            .config
            .issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" issues={issues}"));
    }
    if let Some(runtime) = &snapshot.runtime {
        let internal_route_mismatch = snapshot.config.identity.as_deref()
            != Some(runtime.identity.as_str())
            || snapshot.config.recipient.as_deref() != Some(runtime.recipient.as_str());
        let cursor_route = decode_cursor_strict(&runtime.cursor).ok().flatten();
        let external_route_mismatch = cursor_route.as_ref().is_none_or(|cursor| {
            !cursor_scope_matches(
                snapshot.config.platform,
                snapshot.config.conversation.as_deref(),
                &cursor.external_scope,
            )
        });
        line.push_str(&format!(
            " runtime={} heartbeat={} last_success={} last_delivery={}",
            runtime.status.as_str(),
            runtime.heartbeat_ts,
            runtime.last_success_ts,
            runtime.last_delivery_ts
        ));
        if !runtime.last_error_class.is_empty() {
            line.push_str(&format!(" last_error_class={}", runtime.last_error_class));
        }
        if snapshot.stale {
            line.push_str(" stale=true");
        }
        if internal_route_mismatch {
            line.push_str(" runtime_route_mismatch=true");
        }
        if external_route_mismatch {
            line.push_str(" runtime_external_route_mismatch=true");
        }
    }
    line
}

impl BridgeCallError {
    pub fn new(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
        }
    }
}

/// Produce one platform-safe text message. The final string is bounded in Unicode
/// scalar values and carries an explicit marker when the durable message is longer
/// than the chat surface can represent in one post.
pub fn external_message_text(message: &Message, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let full = format!("[{}] {}", message.sender, message.body);
    if full.chars().count() <= max_chars {
        return full;
    }
    let marker = format!(" … [truncated; message #{}]", message.id);
    let marker_chars = marker.chars().count();
    if marker_chars >= max_chars {
        return marker.chars().take(max_chars).collect();
    }
    let mut out: String = full.chars().take(max_chars - marker_chars).collect();
    out.push_str(&marker);
    out
}

/// Validate a bounded platform JSON response. Both Telegram and Slack use a
/// top-level `ok` boolean; callers pass only the already-bounded body.
pub fn validate_api_response(
    status: u16,
    body: &[u8],
) -> std::result::Result<Value, BridgeCallError> {
    if body.len() > MAX_BRIDGE_RESPONSE_BYTES {
        return Err(BridgeCallError::new(
            "response_too_large",
            format!(
                "bridge API response exceeded {} bytes",
                MAX_BRIDGE_RESPONSE_BYTES
            ),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(BridgeCallError::new(
            "http_status",
            format!("bridge API returned HTTP {status}"),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        BridgeCallError::new("invalid_response", "bridge API returned invalid JSON")
    })?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(BridgeCallError::new(
            "api_rejected",
            // The response is an external, untrusted string and some services or
            // proxies echo request material. Keep persisted/logged diagnostics
            // value-free instead of risking a credential echo through `error` or
            // `description`.
            "bridge API rejected the request",
        ));
    }
    Ok(value)
}

/// Read and validate one reqwest response without permitting an unbounded body.
pub fn read_api_response(
    response: reqwest::blocking::Response,
    max_bytes: usize,
) -> std::result::Result<Value, BridgeCallError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(BridgeCallError::new(
            "response_too_large",
            format!("bridge API response exceeded {max_bytes} bytes"),
        ));
    }
    let mut body = Vec::with_capacity(max_bytes.min(16 * 1024));
    response
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| BridgeCallError::new("response_read", "bridge API response read failed"))?;
    if body.len() > max_bytes {
        return Err(BridgeCallError::new(
            "response_too_large",
            format!("bridge API response exceeded {max_bytes} bytes"),
        ));
    }
    validate_api_response(status, &body)
}

/// Relay oldest unread rows one at a time. The supplied closure represents one
/// fully validated external post (HTTP + application-level success).
pub fn relay_outbound_once<F>(
    store: &dyn Store,
    platform: BridgePlatform,
    identity: &str,
    max_messages: usize,
    mut post: F,
) -> Result<RelayReport>
where
    F: FnMut(&str) -> std::result::Result<(), BridgeCallError>,
{
    let mut report = RelayReport::default();
    let platform_name = platform.as_str();
    for _ in 0..max_messages.min(MAX_BRIDGE_BATCH) {
        let Some(message) = store.peek_oldest_unread(identity)? else {
            break;
        };

        // If the remote accepted this row but the prior local acknowledgement was
        // interrupted, finish only the local acknowledgement on replay.
        let already_relayed = store.has_delivery(
            message.id,
            DeliveryRefKind::Message.as_str(),
            platform_name,
            DeliveryStage::Relayed.as_str(),
            DeliveryOutcome::Ok.as_str(),
        )?;
        let mut relay_trace_failed = false;

        if !already_relayed {
            report.attempted += 1;
            let text = external_message_text(&message, MAX_BRIDGE_TEXT_CHARS);
            if let Err(error) = post(&text) {
                let failure_already_recorded = store.has_delivery(
                    message.id,
                    DeliveryRefKind::Message.as_str(),
                    platform_name,
                    DeliveryStage::RelayFailed.as_str(),
                    DeliveryOutcome::Fail.as_str(),
                )?;
                if !failure_already_recorded {
                    let _ = store.record_delivery(
                        message.id,
                        DeliveryRefKind::Message.as_str(),
                        platform_name,
                        DeliveryStage::RelayFailed.as_str(),
                        DeliveryOutcome::Fail.as_str(),
                    );
                }
                let class = if !error.class.is_empty()
                    && error.class.len() <= 64
                    && error
                        .class
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                {
                    error.class
                } else {
                    "external_post".to_string()
                };
                // A transport closure is allowed to inspect credential-bearing
                // requests and untrusted responses. Never persist/return its raw
                // message even if an implementation accidentally includes one.
                report.error = Some(format!("external bridge post failed ({class})"));
                report.error_class = Some(class);
                break;
            }
            relay_trace_failed = store
                .record_delivery(
                    message.id,
                    DeliveryRefKind::Message.as_str(),
                    platform_name,
                    DeliveryStage::Relayed.as_str(),
                    DeliveryOutcome::Ok.as_str(),
                )
                .is_err();
        }

        if !store.mark_message_read(identity, message.id)? {
            report.error_class = Some("local_ack".to_string());
            report.error = Some(format!(
                "external delivery succeeded but message #{} could not be acknowledged",
                message.id
            ));
            break;
        }
        report.delivered += 1;
        if relay_trace_failed {
            // The external post and exact local acknowledgement both succeeded,
            // so retrying would be wrong. Report the metadata failure honestly
            // after consumption; never include the storage error text or path.
            report.error_class = Some("relay_trace".to_string());
            report.error = Some("delivery succeeded but its relay trace was not recorded".into());
            break;
        }
    }
    report.pending = store.peek_oldest_unread(identity)?.is_some();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(feature = "sqlite")]
    use weave_core::store::SqliteStore;
    #[cfg(feature = "libsql")]
    use weave_core::store_libsql::LibsqlStore;

    #[cfg(feature = "libsql")]
    type TestStore = LibsqlStore;
    #[cfg(feature = "sqlite")]
    type TestStore = SqliteStore;

    static NEXT_DB: AtomicU64 = AtomicU64::new(1);

    fn store() -> (TestStore, PathBuf) {
        let n = NEXT_DB.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("weave-bridge-red-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        #[cfg(feature = "sqlite")]
        let store = SqliteStore::open(&path).expect("bridge SQLite test store");
        #[cfg(feature = "libsql")]
        let store = LibsqlStore::open(&Config {
            backend: Some("libsql".to_string()),
            db: Some(path.to_string_lossy().into_owned()),
            ..Config::default()
        })
        .expect("bridge libSQL test store");
        (store, path)
    }

    fn message_with_body(body: &str) -> Message {
        Message {
            id: 7,
            ts: 1,
            sender: "agent".into(),
            recipient: "telegram".into(),
            subject: None,
            body: body.into(),
            in_reply_to: None,
            idempotency_key: None,
            trace_id: None,
            priority: "normal".into(),
            superseded_by: None,
            expires_at: None,
            kind: Some("message".into()),
            request_priority: None,
            request_ttl: None,
            request_supersedes: None,
            request_dedup_idle: None,
        }
    }

    #[test]
    fn external_text_is_unicode_bounded_and_marks_truncation() {
        let m = message_with_body(&"é".repeat(5_000));
        let text = external_message_text(&m, MAX_BRIDGE_TEXT_CHARS);
        assert!(text.chars().count() <= MAX_BRIDGE_TEXT_CHARS);
        assert!(text.contains("truncated"));
    }

    #[test]
    fn api_response_requires_http_and_application_success() {
        assert!(validate_api_response(500, br#"{"ok":true}"#).is_err());
        assert!(validate_api_response(200, br#"{"ok":false,"error":"bad_auth"}"#).is_err());
        assert!(validate_api_response(200, br#"not json"#).is_err());
        assert!(validate_api_response(200, br#"{"ok":true}"#).is_ok());
        let canary = "response-secret-canary";
        let rejected = validate_api_response(
            200,
            format!(r#"{{"ok":false,"description":"{canary}"}}"#).as_bytes(),
        )
        .unwrap_err();
        assert!(!rejected.message.contains(canary));
        let oversized = vec![b'x'; MAX_BRIDGE_RESPONSE_BYTES + 1];
        let error = validate_api_response(200, &oversized).unwrap_err();
        assert_eq!(error.class, "response_too_large");
    }

    #[test]
    fn failed_external_post_keeps_oldest_message_unread() {
        let (store, path) = store();
        let canary = "transport-secret-canary";
        store
            .send("agent", "telegram", None, "stay unread", None, None)
            .unwrap();
        let report = relay_outbound_once(&store, BridgePlatform::Telegram, "telegram", 1, |_| {
            Err(BridgeCallError::new("api", canary))
        })
        .unwrap();
        assert_eq!(report.delivered, 0);
        assert!(!report.error.as_deref().unwrap_or_default().contains(canary));
        assert_eq!(
            store.peek_oldest_unread("telegram").unwrap().unwrap().body,
            "stay unread"
        );
        relay_outbound_once(&store, BridgePlatform::Telegram, "telegram", 1, |_| {
            Err(BridgeCallError::new("api", "rejected again"))
        })
        .unwrap();
        let trace = store
            .list_delivery(
                store.peek_oldest_unread("telegram").unwrap().unwrap().id,
                100,
            )
            .unwrap();
        assert_eq!(
            trace
                .iter()
                .filter(|row| row.stage == DeliveryStage::RelayFailed.as_str())
                .count(),
            1,
            "repeated retry failures keep one bounded failure marker"
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn confirmed_post_marks_exactly_one_and_records_relay() {
        let (store, path) = store();
        let first = store
            .send("agent", "telegram", None, "first", None, None)
            .unwrap();
        store
            .send("agent", "telegram", None, "second", None, None)
            .unwrap();
        let report =
            relay_outbound_once(&store, BridgePlatform::Telegram, "telegram", 1, |_| Ok(()))
                .unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(
            store.peek_oldest_unread("telegram").unwrap().unwrap().body,
            "second"
        );
        let trace = store.list_delivery(first, 20).unwrap();
        assert!(trace.iter().any(|t| {
            t.stage == DeliveryStage::Relayed.as_str()
                && t.outcome == DeliveryOutcome::Ok.as_str()
                && t.ref_kind == DeliveryRefKind::Message.as_str()
        }));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recorded_remote_acceptance_is_acknowledged_without_reposting() {
        let (store, path) = store();
        let message_id = store
            .send("agent", "telegram", None, "already accepted", None, None)
            .unwrap();
        for _ in 0..(weave_core::model::MAX_DELIVERY_ROWS + 5) {
            store
                .record_delivery(
                    message_id,
                    DeliveryRefKind::Message.as_str(),
                    BridgePlatform::Telegram.as_str(),
                    DeliveryStage::RelayFailed.as_str(),
                    DeliveryOutcome::Fail.as_str(),
                )
                .unwrap();
        }
        store
            .record_delivery(
                message_id,
                DeliveryRefKind::Message.as_str(),
                BridgePlatform::Telegram.as_str(),
                DeliveryStage::Relayed.as_str(),
                DeliveryOutcome::Ok.as_str(),
            )
            .unwrap();

        let mut post_calls = 0;
        let report = relay_outbound_once(&store, BridgePlatform::Telegram, "telegram", 1, |_| {
            post_calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(post_calls, 0);
        assert_eq!(report.attempted, 0);
        assert_eq!(report.delivered, 1);
        assert!(store.peek_oldest_unread("telegram").unwrap().is_none());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn status_snapshot_is_network_free_and_never_serializes_token() {
        let (store, path) = store();
        let secret = "telegram-status-secret";
        let config = Config {
            telegram_token: Some(secret.to_string()),
            telegram_chat_id: Some("chat-1".to_string()),
            telegram_identity: Some("telegram-bridge".to_string()),
            telegram_recipient: Some("agent".to_string()),
            ..Config::default()
        };
        let snapshot = bridge_status_snapshot(&store, &config, BridgePlatform::Telegram).unwrap();
        assert!(snapshot.config.ready);
        assert!(!snapshot.active);
        assert!(!snapshot.healthy);
        assert_eq!(bridge_status_token(&snapshot), "ready_inactive");
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains(secret));
        assert!(!bridge_status_line(&snapshot).contains(secret));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn active_runtime_on_an_old_route_is_degraded_not_healthy() {
        let (store, path) = store();
        store
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "old-bridge",
                "old-agent",
                "test-owner",
                Some(i64::from(std::process::id())),
                "test-host",
                0,
            )
            .unwrap()
            .expect("claim bridge runtime");
        store
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                "test-owner",
                &weave_core::model::BridgeRuntimeUpdate {
                    status: Some(BridgeRuntimeStatus::Running),
                    error: weave_core::model::BridgeRuntimeErrorUpdate::Clear,
                    ..weave_core::model::BridgeRuntimeUpdate::default()
                },
            )
            .unwrap();
        let config = Config {
            telegram_token: Some("token".to_string()),
            telegram_chat_id: Some("chat".to_string()),
            telegram_identity: Some("new-bridge".to_string()),
            telegram_recipient: Some("new-agent".to_string()),
            ..Config::default()
        };

        let snapshot = bridge_status_snapshot(&store, &config, BridgePlatform::Telegram).unwrap();
        assert!(snapshot.active);
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.status, "degraded");
        let line = bridge_status_line(&snapshot);
        assert!(line.contains("runtime_route_mismatch=true"), "{line}");
        assert!(!line.contains("old-bridge"), "{line}");
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn active_runtime_on_an_old_external_scope_is_degraded() {
        let (store, path) = store();
        store
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "agent",
                "scope-owner",
                Some(i64::from(std::process::id())),
                "test-host",
                0,
            )
            .unwrap()
            .expect("claim bridge runtime");
        let cursor = weave_core::model::BridgeCursorEnvelope {
            external_identity: "bot-1".to_string(),
            external_scope: "old-chat".to_string(),
            position: "42".to_string(),
            continuation: None,
        }
        .encode()
        .unwrap();
        store
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                "scope-owner",
                &weave_core::model::BridgeRuntimeUpdate {
                    cursor: Some(cursor),
                    status: Some(BridgeRuntimeStatus::Running),
                    error: weave_core::model::BridgeRuntimeErrorUpdate::Clear,
                    ..weave_core::model::BridgeRuntimeUpdate::default()
                },
            )
            .unwrap();
        let config = Config {
            telegram_token: Some("token".to_string()),
            telegram_chat_id: Some("new-chat".to_string()),
            telegram_identity: Some("telegram-bridge".to_string()),
            telegram_recipient: Some("agent".to_string()),
            ..Config::default()
        };
        let snapshot = bridge_status_snapshot(&store, &config, BridgePlatform::Telegram).unwrap();
        assert!(snapshot.active);
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.status, "degraded");
        let line = bridge_status_line(&snapshot);
        assert!(
            line.contains("runtime_external_route_mismatch=true"),
            "{line}"
        );
        assert!(!line.contains("old-chat"), "{line}");
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn owned_runtime_without_a_live_process_is_explicitly_stale() {
        let (store, path) = store();
        store
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                "telegram-bridge",
                "agent",
                "stale-owner",
                Some(7),
                "test-host",
                0,
            )
            .unwrap()
            .expect("claim bridge runtime");
        let cursor = weave_core::model::BridgeCursorEnvelope {
            external_identity: "bot-1".to_string(),
            external_scope: "chat".to_string(),
            position: "42".to_string(),
            continuation: None,
        }
        .encode()
        .unwrap();
        store
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                "stale-owner",
                &weave_core::model::BridgeRuntimeUpdate {
                    cursor: Some(cursor),
                    status: Some(BridgeRuntimeStatus::Running),
                    error: weave_core::model::BridgeRuntimeErrorUpdate::Clear,
                    ..weave_core::model::BridgeRuntimeUpdate::default()
                },
            )
            .unwrap();
        let config = Config {
            telegram_token: Some("token".to_string()),
            telegram_chat_id: Some("chat".to_string()),
            telegram_identity: Some("telegram-bridge".to_string()),
            telegram_recipient: Some("agent".to_string()),
            ..Config::default()
        };

        let snapshot = bridge_status_snapshot_at(
            &store,
            &config,
            BridgePlatform::Telegram,
            weave_core::model::now().saturating_add(weave_core::model::BRIDGE_ACTIVE_TTL_SECS + 1),
        )
        .unwrap();
        assert!(!snapshot.active);
        assert!(snapshot.stale);
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.status, "stale");
        assert!(bridge_status_line(&snapshot).contains("stale=true"));
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
