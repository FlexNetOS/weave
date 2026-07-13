//! WL-048 / ADR-0004: bounded, poll-only Slack bridge.
//!
//! The bridge owns one configured Slack channel. Inbound human text is persisted
//! as the configured bridge identity, outbound mesh rows remain unread until Slack
//! confirms `chat.postMessage`, and a route-bound cursor survives restarts without
//! being reused after an account/channel change.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::time::Duration;
use weave_core::config::{Config, SlackBridgeRuntimeConfig};
use weave_core::model::{
    self, BridgeCursorEnvelope, BridgePlatform, BridgeRuntimeErrorUpdate, BridgeRuntimeStatus,
    BridgeRuntimeUpdate, BridgeStagedEvent,
};
use weave_core::store::{check_body, Store, MAX_BODY};

use crate::telegram::{
    dispatch_bot_command_with_key, parse_bot_command, sanitize_inbound_ident, BotCommand,
    BotDispatchDisposition, BotDispatchOutcome,
};

const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Slack can impose a one-request-per-minute history tier. Poll conservatively by
/// default so a healthy bridge does not create its own rate-limit condition.
const POLL_SECS: u64 = 60;
const MAX_RETRY_AFTER_SECS: u64 = 15 * 60;
// Slack's strictest current `conversations.history` tier permits 15 objects and
// one request per minute. Use that portable floor even for workspaces whose
// internal apps receive the more generous Tier 3 allowance.
const HISTORY_PAGE_SIZE: u16 = 15;
const BOOTSTRAP_PAGE_SIZE: u16 = 1;
const MAX_HISTORY_PAGES_PER_ITERATION: usize = 1;
const MAX_HISTORY_MESSAGES: usize = HISTORY_PAGE_SIZE as usize;
const MAX_STAGED_DRAIN_PER_ITERATION: usize = 50;
const MAX_OUTBOUND_POSTS_PER_ITERATION: usize = 1;
const HEARTBEAT_SECS: u64 = 30;

/// Build the JSON body for Slack `chat.postMessage`. Pure — no network.
pub fn slack_post_payload(channel: &str, text: &str) -> Value {
    json!({ "channel": channel, "text": text })
}

/// Build a `conversations.history` request shape. Slack's timestamp boundaries
/// are deliberately exclusive.
#[cfg(test)]
pub fn slack_history_payload(channel: &str, oldest: &str, latest: Option<&str>) -> Value {
    let mut payload = json!({
        "channel": channel,
        "oldest": oldest,
        "inclusive": false,
        "limit": HISTORY_PAGE_SIZE,
    });
    if let Some(latest) = latest {
        payload["latest"] = json!(latest);
    }
    payload
}

/// Parse a normal human Slack message into `(external sender, text)`. Bot echoes,
/// edits, joins, and other subtype events are intentionally ignored.
pub fn parse_slack_message(msg: &Value) -> Option<(String, String)> {
    if msg.get("subtype").is_some() || msg.get("bot_id").is_some() {
        return None;
    }
    let text = msg.get("text")?.as_str()?.to_string();
    if text.is_empty() {
        return None;
    }
    let from = msg
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "slack-user".to_string());
    Some((from, text))
}

/// Parse a Slack history message as a weave bot command. Slack clients reserve
/// leading `/` text for registered slash commands, so `!weave …` is the usable
/// ordinary-message form that survives into `conversations.history`. Legacy
/// slash text remains accepted for compatibility when it does reach history.
pub fn parse_slack_command(text: &str) -> Option<BotCommand> {
    if text == "!weave" {
        return Some(BotCommand::Help);
    }
    if let Some(rest) = text.strip_prefix("!weave") {
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return None;
        }
        let command = rest.trim_start_matches(char::is_whitespace);
        if command.is_empty() {
            return Some(BotCommand::Help);
        }
        return parse_bot_command(&format!("/{command}"));
    }
    parse_bot_command(text)
}

fn slack_help_text() -> String {
    "weave Slack commands (send these as ordinary channel messages):\n\
     !weave inbox — unread messages for this bridge\n\
     !weave peers — registered peers + presence\n\
     !weave sessions — known sessions + unread counts\n\
     !weave send <to> <body> — send a message (requires WEAVE_BOT_WRITES=1)\n\
     !weave ask <to> <body> — open a tracked ask (requires WEAVE_BOT_WRITES=1)\n\
     !weave answer <ask_id> <body> — answer an ask (requires WEAVE_BOT_WRITES=1)\n\
     !weave reply <message_id> <body> — reply (requires WEAVE_BOT_WRITES=1)\n\
     !weave help — this list\n\
     Legacy /command text is also accepted when it reaches Slack history."
        .to_string()
}

/// Exact Slack timestamp representation. Decimal strings are compared without a
/// floating-point conversion, so adjacent microsecond event ids never collapse.
#[derive(Debug, Clone)]
struct SlackTs {
    raw: String,
    seconds: String,
    fraction: String,
}

impl PartialEq for SlackTs {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SlackTs {}

impl SlackTs {
    fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty()
            || raw.chars().count() > model::MAX_BRIDGE_POSITION_LEN
            || raw.chars().any(char::is_control)
        {
            return None;
        }
        let (seconds, fraction) = raw.split_once('.')?;
        if seconds.is_empty()
            || fraction.is_empty()
            || fraction.contains('.')
            || !seconds.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        // Bound comparison work independently of the outer cursor cap. Slack uses
        // ten seconds digits and six fractional digits today; these are generous.
        if seconds.len() > 20 || fraction.len() > 18 {
            return None;
        }
        let normalized_seconds = seconds.trim_start_matches('0');
        Some(Self {
            raw: raw.to_string(),
            seconds: if normalized_seconds.is_empty() {
                "0".to_string()
            } else {
                normalized_seconds.to_string()
            },
            fraction: fraction.to_string(),
        })
    }

    fn cmp_fraction(left: &str, right: &str) -> Ordering {
        let width = left.len().max(right.len());
        for index in 0..width {
            let left_digit = left.as_bytes().get(index).copied().unwrap_or(b'0');
            let right_digit = right.as_bytes().get(index).copied().unwrap_or(b'0');
            match left_digit.cmp(&right_digit) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }

    fn order_key(&self) -> String {
        // Parsing already caps these components at 20 and 18 decimal digits.
        // Padding makes ordinary SQL text order identical to `Ord` above.
        format!("{:0>20}.{:0<18}", self.seconds, self.fraction)
    }
}

impl Ord for SlackTs {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seconds
            .len()
            .cmp(&other.seconds.len())
            .then_with(|| self.seconds.cmp(&other.seconds))
            .then_with(|| Self::cmp_fraction(&self.fraction, &other.fraction))
    }
}

impl PartialOrd for SlackTs {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Extract the greatest valid Slack timestamp exactly (no `f64`).
pub fn latest_ts(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .filter_map(|message| message.get("ts").and_then(Value::as_str))
        .filter_map(SlackTs::parse)
        .max()
        .map(|timestamp| timestamp.raw)
}

fn staged_event(message: &Value) -> SlackApiResult<BridgeStagedEvent> {
    let raw = message.get("ts").and_then(Value::as_str).ok_or_else(|| {
        SlackApiError::new(
            "invalid_response",
            "Slack history returned an invalid timestamp",
        )
    })?;
    let timestamp = SlackTs::parse(raw).ok_or_else(|| {
        SlackApiError::new(
            "invalid_response",
            "Slack history returned an invalid timestamp",
        )
    })?;
    let (sender, text) = match parse_slack_message(message) {
        Some((sender, text)) => (
            Some(sanitize_inbound_ident(&sender, "slack-user")),
            Some(text),
        ),
        None => (None, None),
    };
    let event = BridgeStagedEvent {
        position: timestamp.raw.clone(),
        order_key: timestamp.order_key(),
        sender,
        text,
    };
    event
        .validate()
        .map_err(|_| SlackApiError::new("invalid_response", "Slack history event is too large"))?;
    Ok(event)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackApiError {
    class: String,
    message: String,
    retry_after_secs: Option<u64>,
}

impl std::fmt::Display for SlackApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.class, self.message)
    }
}

impl std::error::Error for SlackApiError {}

impl SlackApiError {
    fn new(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
            retry_after_secs: None,
        }
    }

    fn rate_limited(retry_after_secs: Option<u64>) -> Self {
        Self {
            class: "rate_limited".to_string(),
            message: "Slack API rate limit reached".to_string(),
            retry_after_secs: retry_after_secs
                .map(|seconds| seconds.clamp(POLL_SECS, MAX_RETRY_AFTER_SECS)),
        }
    }

    fn from_bridge(error: crate::bridge::BridgeCallError) -> Self {
        Self::new(error.class, error.message)
    }

    fn into_bridge(self) -> crate::bridge::BridgeCallError {
        crate::bridge::BridgeCallError::new(self.class, self.message)
    }
}

type SlackApiResult<T> = std::result::Result<T, SlackApiError>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackAuth {
    external_identity: String,
}

#[derive(Debug, Clone)]
struct SlackHistoryRequest<'a> {
    channel: &'a str,
    oldest: &'a str,
    latest: Option<&'a str>,
    limit: u16,
}

#[derive(Debug, Clone)]
struct SlackHistoryPage {
    messages: Vec<Value>,
    has_more: bool,
}

trait SlackApi {
    fn auth_test(&mut self) -> SlackApiResult<SlackAuth>;
    fn history(&mut self, request: &SlackHistoryRequest<'_>) -> SlackApiResult<SlackHistoryPage>;
    fn post_message(&mut self, channel: &str, text: &str) -> SlackApiResult<()>;
}

struct ReqwestSlackApi {
    client: reqwest::blocking::Client,
    token: String,
}

impl ReqwestSlackApi {
    fn new(token: String) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .context("building Slack HTTP client")?;
        Ok(Self { client, token })
    }

    fn request_error(error: &reqwest::Error) -> SlackApiError {
        let (class, message) = if error.is_timeout() {
            ("timeout", "Slack API request timed out")
        } else if error.is_connect() {
            ("connect", "Slack API connection failed")
        } else if error.is_decode() {
            ("decode", "Slack API response decoding failed")
        } else {
            ("request", "Slack API request failed")
        };
        SlackApiError::new(class, message)
    }

    fn read_response(
        response: reqwest::blocking::Response,
        max_bytes: usize,
    ) -> SlackApiResult<Value> {
        let status = response.status().as_u16();
        if status == 429 {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            return Err(SlackApiError::rate_limited(retry_after));
        }
        crate::bridge::read_api_response(response, max_bytes).map_err(SlackApiError::from_bridge)
    }
}

impl SlackApi for ReqwestSlackApi {
    fn auth_test(&mut self) -> SlackApiResult<SlackAuth> {
        let response = self
            .client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(&self.token)
            .send()
            .map_err(|error| Self::request_error(&error))?;
        let value = Self::read_response(response, crate::bridge::MAX_BRIDGE_POST_RESPONSE_BYTES)?;
        parse_auth(&value)
    }

    fn history(&mut self, request: &SlackHistoryRequest<'_>) -> SlackApiResult<SlackHistoryPage> {
        let limit = request.limit.to_string();
        let mut query = vec![
            ("channel", request.channel),
            ("oldest", request.oldest),
            ("inclusive", "false"),
            ("limit", limit.as_str()),
        ];
        if let Some(latest) = request.latest {
            query.push(("latest", latest));
        }
        let response = self
            .client
            .get("https://slack.com/api/conversations.history")
            .bearer_auth(&self.token)
            .query(&query)
            .send()
            .map_err(|error| Self::request_error(&error))?;
        let value = Self::read_response(response, crate::bridge::MAX_BRIDGE_RESPONSE_BYTES)?;
        parse_history_page(&value, request.limit as usize)
    }

    fn post_message(&mut self, channel: &str, text: &str) -> SlackApiResult<()> {
        let response = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.token)
            .json(&slack_post_payload(channel, text))
            .send()
            .map_err(|error| Self::request_error(&error))?;
        Self::read_response(response, crate::bridge::MAX_BRIDGE_POST_RESPONSE_BYTES)?;
        Ok(())
    }
}

fn bounded_route_field(label: &str, value: Option<&str>) -> SlackApiResult<String> {
    let value = value.ok_or_else(|| {
        SlackApiError::new(
            "invalid_response",
            format!("Slack auth response omitted {label}"),
        )
    })?;
    if value.is_empty()
        || value.chars().count() > model::MAX_BRIDGE_ROUTE_FIELD_LEN
        || value.chars().any(char::is_control)
    {
        return Err(SlackApiError::new(
            "invalid_response",
            format!("Slack auth response contained an invalid {label}"),
        ));
    }
    Ok(value.to_string())
}

fn parse_auth(value: &Value) -> SlackApiResult<SlackAuth> {
    let user_id = bounded_route_field("user id", value.get("user_id").and_then(Value::as_str))?;
    let team_id = bounded_route_field("team id", value.get("team_id").and_then(Value::as_str))?;
    let external_identity = format!("{team_id}:{user_id}");
    if external_identity.chars().count() > model::MAX_BRIDGE_ROUTE_FIELD_LEN {
        return Err(SlackApiError::new(
            "invalid_response",
            "Slack auth identity is too long",
        ));
    }
    Ok(SlackAuth { external_identity })
}

fn parse_history_page(value: &Value, requested_limit: usize) -> SlackApiResult<SlackHistoryPage> {
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| SlackApiError::new("invalid_response", "Slack history omitted messages"))?;
    let max_messages = requested_limit.min(MAX_HISTORY_MESSAGES);
    if messages.len() > max_messages {
        return Err(SlackApiError::new(
            "invalid_response",
            "Slack history returned too many messages",
        ));
    }
    for message in messages {
        let valid_ts = message
            .get("ts")
            .and_then(Value::as_str)
            .and_then(SlackTs::parse)
            .is_some();
        if !valid_ts {
            return Err(SlackApiError::new(
                "invalid_response",
                "Slack history returned an invalid timestamp",
            ));
        }
    }
    let has_more = value
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(|| SlackApiError::new("invalid_response", "Slack history omitted has_more"))?;
    if has_more && messages.is_empty() {
        return Err(SlackApiError::new(
            "invalid_response",
            "Slack history claimed more pages after an empty page",
        ));
    }
    Ok(SlackHistoryPage {
        messages: messages.clone(),
        has_more,
    })
}

/// Validate a bounded Slack response body. Kept separate so malformed, rejected,
/// oversized, and rate-limited responses are testable without an endpoint.
#[cfg(test)]
fn validate_slack_bytes(
    status: u16,
    body: &[u8],
    retry_after_secs: Option<u64>,
) -> SlackApiResult<Value> {
    if status == 429 {
        return Err(SlackApiError::rate_limited(retry_after_secs));
    }
    crate::bridge::validate_api_response(status, body).map_err(SlackApiError::from_bridge)
}

fn check_with_api<A: SlackApi + ?Sized>(
    runtime: &SlackBridgeRuntimeConfig,
    api: &mut A,
) -> SlackApiResult<crate::bridge::BridgeCheck> {
    let auth = api.auth_test()?;
    api.history(&SlackHistoryRequest {
        channel: &runtime.channel,
        oldest: "0",
        latest: None,
        limit: BOOTSTRAP_PAGE_SIZE,
    })
    .map_err(|_| {
        SlackApiError::new("channel_read", "Slack configured channel could not be read")
    })?;
    Ok(crate::bridge::BridgeCheck {
        platform: BridgePlatform::Slack,
        external_identity: Some(auth.external_identity),
        external_scope: Some(runtime.channel.clone()),
    })
}

/// Perform a bounded, non-consuming Slack `auth.test` plus one configured-channel
/// history read. This validates identity and read access only; it does not post a
/// message and therefore does not claim `chat:write` capability.
pub fn check(config: &Config) -> Result<crate::bridge::BridgeCheck> {
    let runtime = config.slack_bridge_runtime()?;
    let mut api = ReqwestSlackApi::new(runtime.token.clone())?;
    check_with_api(&runtime, &mut api)
        .map_err(|error| anyhow::anyhow!("{}: {}", error.class, error.message))
}

fn new_envelope(auth: &SlackAuth, channel: &str) -> BridgeCursorEnvelope {
    BridgeCursorEnvelope {
        external_identity: auth.external_identity.clone(),
        external_scope: channel.to_string(),
        position: String::new(),
        continuation: None,
    }
}

fn load_envelope(
    stored: &str,
    auth: &SlackAuth,
    channel: &str,
) -> SlackApiResult<BridgeCursorEnvelope> {
    let decoded =
        crate::bridge::decode_cursor_strict(stored).map_err(SlackApiError::from_bridge)?;
    let Some(mut envelope) = decoded else {
        return Ok(new_envelope(auth, channel));
    };
    if !envelope.route_matches(&auth.external_identity, channel) {
        return Ok(new_envelope(auth, channel));
    }
    let valid_position = envelope.position == "0"
        || envelope.position.is_empty()
        || SlackTs::parse(&envelope.position).is_some();
    if !valid_position {
        return Err(SlackApiError::new(
            "cursor",
            "Slack cursor position is invalid",
        ));
    }

    // Older releases persisted Slack's opaque `next_cursor` here. Only a valid
    // timestamp strictly above the durable watermark is a resumable time bound.
    let valid_continuation = match envelope.continuation.as_deref() {
        None => true,
        Some(_) if envelope.position.is_empty() => false,
        Some(raw) => SlackTs::parse(raw).is_some_and(|bound| {
            envelope.position == "0"
                || SlackTs::parse(&envelope.position).is_some_and(|position| bound > position)
        }),
    };
    if !valid_continuation {
        envelope.continuation = None;
    }
    Ok(envelope)
}

fn encoded_cursor(envelope: &BridgeCursorEnvelope) -> SlackApiResult<String> {
    envelope
        .encode()
        .map_err(|_| SlackApiError::new("cursor", "Slack cursor state is invalid"))
}

fn update_owned(
    store: &dyn Store,
    owner_id: &str,
    update: &BridgeRuntimeUpdate,
) -> SlackApiResult<()> {
    match store.update_bridge_runtime(BridgePlatform::Slack, owner_id, update) {
        Ok(true) => Ok(()),
        Ok(false) => Err(SlackApiError::new(
            "ownership_lost",
            "Slack bridge runtime ownership was lost",
        )),
        Err(_) => Err(SlackApiError::new(
            "store",
            "Slack bridge runtime state update failed",
        )),
    }
}

fn heartbeat_owned(store: &dyn Store, owner_id: &str) -> SlackApiResult<()> {
    update_owned(store, owner_id, &BridgeRuntimeUpdate::default())
}

fn owned_history<A: SlackApi>(
    store: &dyn Store,
    owner_id: &str,
    api: &mut A,
    request: &SlackHistoryRequest<'_>,
) -> SlackApiResult<SlackHistoryPage> {
    // Refresh the lease immediately before the bounded request. The request may
    // consume most of its timeout, so every later side effect fences again.
    heartbeat_owned(store, owner_id)?;
    api.history(request)
}

fn owned_post<A: SlackApi>(
    store: &dyn Store,
    owner_id: &str,
    api: &mut A,
    channel: &str,
    text: &str,
) -> SlackApiResult<()> {
    heartbeat_owned(store, owner_id)?;
    api.post_message(channel, text)?;
    // A remote acceptance may outlive the request-side heartbeat. Re-fence before
    // any caller records/acknowledges that side effect so a reclaimed worker can
    // never consume local work after losing ownership.
    heartbeat_owned(store, owner_id)
}

fn wait_owned(store: &dyn Store, owner_id: &str, delay: Duration) -> SlackApiResult<()> {
    let mut remaining = delay;
    let heartbeat_interval = Duration::from_secs(HEARTBEAT_SECS);
    while !remaining.is_zero() {
        let step = remaining.min(heartbeat_interval);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
        // Keep a healthy, rate-limited worker well inside the 90-second runtime
        // TTL even when Slack asks for a long retry delay.
        if !remaining.is_zero() {
            heartbeat_owned(store, owner_id)?;
        }
    }
    Ok(())
}

fn persist_success(
    store: &dyn Store,
    owner_id: &str,
    envelope: &BridgeCursorEnvelope,
    delivered: bool,
) -> SlackApiResult<()> {
    let update = success_update(envelope, delivered)?;
    update_owned(store, owner_id, &update)
}

fn success_update(
    envelope: &BridgeCursorEnvelope,
    delivered: bool,
) -> SlackApiResult<BridgeRuntimeUpdate> {
    let timestamp = model::now();
    Ok(BridgeRuntimeUpdate {
        cursor: Some(encoded_cursor(envelope)?),
        status: Some(BridgeRuntimeStatus::Running),
        last_poll_ts: Some(timestamp),
        last_success_ts: Some(timestamp),
        last_delivery_ts: delivered.then_some(timestamp),
        error: BridgeRuntimeErrorUpdate::Clear,
    })
}

fn persist_failure(store: &dyn Store, owner_id: &str, error: &SlackApiError) -> Result<()> {
    let updated = store.update_bridge_runtime(
        BridgePlatform::Slack,
        owner_id,
        &BridgeRuntimeUpdate {
            status: Some(BridgeRuntimeStatus::Degraded),
            last_poll_ts: Some(model::now()),
            error: BridgeRuntimeErrorUpdate::Set {
                class: error.class.clone(),
                message: error.message.clone(),
            },
            ..BridgeRuntimeUpdate::default()
        },
    )?;
    if !updated {
        anyhow::bail!("Slack bridge runtime ownership was lost");
    }
    Ok(())
}

fn bound_external_text(text: &str) -> String {
    if text.chars().count() <= crate::bridge::MAX_BRIDGE_TEXT_CHARS {
        return text.to_string();
    }
    const MARKER: &str = " … [truncated]";
    let marker_chars = MARKER.chars().count();
    let mut bounded: String = text
        .chars()
        .take(crate::bridge::MAX_BRIDGE_TEXT_CHARS.saturating_sub(marker_chars))
        .collect();
    bounded.push_str(MARKER);
    bounded
}

fn inbound_body(raw_from: &str, text: &str, fallback: &str) -> String {
    let from = sanitize_inbound_ident(raw_from, fallback);
    let prefix = format!("[Slack {from}] ");
    let available = MAX_BODY.saturating_sub(prefix.len());
    let mut end = text.len().min(available);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut body = prefix;
    body.push_str(&text[..end]);
    body
}

fn inbound_idempotency_key(envelope: &BridgeCursorEnvelope, timestamp: &str) -> String {
    // FNV-1a is sufficient here: this is a compact deterministic namespace, not a
    // credential digest or authenticity decision. The exact event timestamp remains
    // visible for diagnostics and keeps the result well under the store cap.
    let mut hash = 0xcbf29ce484222325u64;
    for component in [
        envelope.external_identity.as_bytes(),
        envelope.external_scope.as_bytes(),
    ] {
        for byte in component {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("slack:{hash:016x}:{timestamp}")
}

fn relay_inbound(
    store: &dyn Store,
    runtime: &SlackBridgeRuntimeConfig,
    envelope: &BridgeCursorEnvelope,
    raw_from: &str,
    text: &str,
    timestamp: &str,
) -> SlackApiResult<()> {
    let body = inbound_body(raw_from, text, &runtime.identity);
    check_body(&body)
        .map_err(|_| SlackApiError::new("invalid_message", "Slack inbound message is too long"))?;
    let idempotency_key = inbound_idempotency_key(envelope, timestamp);
    let accepted = store.send_idempotent(
        &runtime.identity,
        &runtime.recipient,
        None,
        &body,
        Some(&idempotency_key),
        None,
    );
    match accepted {
        Ok(_) => Ok(()),
        Err(_) => {
            // Slack can return an edited representation under the same immutable
            // message timestamp. If the first relay committed before a crash but
            // the cursor did not, preserve the first accepted body and let the
            // cursor advance instead of wedging forever on a content mismatch.
            let replay = store
                .message_by_idempotency_key(&idempotency_key)
                .map_err(|_| {
                    SlackApiError::new("store", "Slack inbound message could not be stored")
                })?;
            // The provider route + timestamp is the authoritative event identity.
            // A first representation may have dispatched a command (and therefore
            // created a differently-shaped message) before the cursor persisted;
            // an edited ordinary representation must still resolve to that first
            // accepted event rather than wedge the bridge.
            if replay.is_some() {
                Ok(())
            } else {
                Err(SlackApiError::new(
                    "store",
                    "Slack inbound message could not be stored",
                ))
            }
        }
    }
}

fn advance_position(
    store: &dyn Store,
    owner_id: &str,
    envelope: &mut BridgeCursorEnvelope,
    timestamp: &str,
    delivered: bool,
) -> SlackApiResult<()> {
    let mut next = envelope.clone();
    next.position = timestamp.to_string();
    next.continuation = None;
    persist_success(store, owner_id, &next, delivered)?;
    *envelope = next;
    Ok(())
}

fn stage_history_events(
    store: &dyn Store,
    owner_id: &str,
    envelope: &mut BridgeCursorEnvelope,
    next: BridgeCursorEnvelope,
    events: &[BridgeStagedEvent],
) -> SlackApiResult<()> {
    let update = success_update(&next, false)?;
    match store.stage_bridge_events(
        BridgePlatform::Slack,
        owner_id,
        &envelope.external_identity,
        &envelope.external_scope,
        events,
        &update,
    ) {
        Ok(true) => {
            *envelope = next;
            Ok(())
        }
        Ok(false) => Err(SlackApiError::new(
            "ownership_lost",
            "Slack bridge runtime ownership was lost",
        )),
        Err(_) => Err(SlackApiError::new(
            "store",
            "Slack history page could not be staged",
        )),
    }
}

fn complete_staged_position(
    store: &dyn Store,
    owner_id: &str,
    envelope: &mut BridgeCursorEnvelope,
    staged_position: &str,
    next_position: Option<&str>,
    delivered: bool,
) -> SlackApiResult<()> {
    let mut next = envelope.clone();
    if let Some(position) = next_position {
        next.position = position.to_string();
    }
    next.continuation = None;
    let update = success_update(&next, delivered)?;
    match store.complete_bridge_staged_event(
        BridgePlatform::Slack,
        owner_id,
        &envelope.external_identity,
        &envelope.external_scope,
        staged_position,
        &update,
    ) {
        Ok(true) => {
            *envelope = next;
            Ok(())
        }
        Ok(false) => Err(SlackApiError::new(
            "ownership_lost",
            "Slack bridge runtime ownership was lost",
        )),
        Err(_) => Err(SlackApiError::new(
            "store",
            "Slack staged event could not be completed",
        )),
    }
}

fn prepare_staging_owned(
    store: &dyn Store,
    owner_id: &str,
    envelope: &BridgeCursorEnvelope,
) -> SlackApiResult<()> {
    match store.prepare_bridge_staging(
        BridgePlatform::Slack,
        owner_id,
        &envelope.external_identity,
        &envelope.external_scope,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err(SlackApiError::new(
            "ownership_lost",
            "Slack bridge runtime ownership was lost",
        )),
        Err(_) => Err(SlackApiError::new(
            "store",
            "Slack staged-event route could not be prepared",
        )),
    }
}

fn peek_staged_event(
    store: &dyn Store,
    envelope: &BridgeCursorEnvelope,
) -> SlackApiResult<Option<BridgeStagedEvent>> {
    store
        .peek_bridge_staged_event(
            BridgePlatform::Slack,
            &envelope.external_identity,
            &envelope.external_scope,
        )
        .map_err(|_| SlackApiError::new("store", "Slack staged event could not be read"))
}

#[derive(Debug)]
struct InboxSnapshot {
    message_id: Option<i64>,
    reply: String,
}

fn inbox_snapshot(
    store: &dyn Store,
    runtime: &SlackBridgeRuntimeConfig,
) -> SlackApiResult<InboxSnapshot> {
    let row = store
        .peek_oldest_unread(&runtime.identity)
        .map_err(|_| SlackApiError::new("store", "Slack bridge inbox could not be read"))?;
    let unread = store
        .unread_count(&runtime.identity)
        .map_err(|_| SlackApiError::new("store", "Slack bridge inbox count failed"))?;
    let reply = match &row {
        Some(message) => weave_mcp::mcp::format_inbox_rows(
            &runtime.identity,
            std::slice::from_ref(message),
            unread,
            false,
            false,
        ),
        None => weave_mcp::mcp::format_inbox_rows(&runtime.identity, &[], 0, false, false),
    };
    Ok(InboxSnapshot {
        message_id: row.map(|message| message.id),
        reply: bound_external_text(&reply),
    })
}

fn acknowledge_inbox_snapshot(
    store: &dyn Store,
    runtime: &SlackBridgeRuntimeConfig,
    owner_id: &str,
    message_id: Option<i64>,
) -> SlackApiResult<()> {
    let Some(message_id) = message_id else {
        return Ok(());
    };
    heartbeat_owned(store, owner_id)?;
    let acknowledged = store
        .mark_message_read(&runtime.identity, message_id)
        .map_err(|_| {
            SlackApiError::new("local_ack", "Slack inbox reply could not be acknowledged")
        })?;
    if !acknowledged {
        return Err(SlackApiError::new(
            "local_ack",
            "Slack inbox reply could not be acknowledged",
        ));
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SlackMessageReport {
    delivered: bool,
    command_replied: bool,
}

#[allow(clippy::too_many_arguments)]
fn handle_history_message<A, D>(
    store: &dyn Store,
    api: &mut A,
    runtime: &SlackBridgeRuntimeConfig,
    owner_id: &str,
    envelope: &mut BridgeCursorEnvelope,
    event: &BridgeStagedEvent,
    dispatch: &mut D,
) -> SlackApiResult<SlackMessageReport>
where
    A: SlackApi,
    D: FnMut(&BotCommand, Option<&str>) -> BotDispatchOutcome,
{
    let timestamp = event.position.as_str();
    let (Some(from), Some(text)) = (event.sender.as_deref(), event.text.as_deref()) else {
        complete_staged_position(store, owner_id, envelope, timestamp, Some(timestamp), false)?;
        return Ok(SlackMessageReport::default());
    };
    if let Some(command) = parse_slack_command(text) {
        if command == BotCommand::Inbox {
            let snapshot = inbox_snapshot(store, runtime)?;
            owned_post(store, owner_id, api, &runtime.channel, &snapshot.reply)?;
            // Provider acceptance is durable before exact local acknowledgement.
            // An ack failure therefore cannot replay the same `/inbox` response.
            complete_staged_position(store, owner_id, envelope, timestamp, Some(timestamp), true)?;
            acknowledge_inbox_snapshot(store, runtime, owner_id, snapshot.message_id)?;
            return Ok(SlackMessageReport {
                delivered: true,
                command_replied: true,
            });
        }
        let event_key = inbound_idempotency_key(envelope, timestamp);
        heartbeat_owned(store, owner_id)?;
        let outcome = if command == BotCommand::Help {
            BotDispatchOutcome {
                reply: slack_help_text(),
                disposition: BotDispatchDisposition::Terminal,
            }
        } else {
            dispatch(&command, Some(&event_key))
        };
        let reply = bound_external_text(&outcome.reply);
        match outcome.disposition {
            BotDispatchDisposition::DurableMutation => {
                complete_staged_position(
                    store,
                    owner_id,
                    envelope,
                    timestamp,
                    Some(timestamp),
                    false,
                )?;
                owned_post(store, owner_id, api, &runtime.channel, &reply)?;
                persist_success(store, owner_id, envelope, true)?;
            }
            BotDispatchDisposition::Terminal => {
                owned_post(store, owner_id, api, &runtime.channel, &reply)?;
                complete_staged_position(
                    store,
                    owner_id,
                    envelope,
                    timestamp,
                    Some(timestamp),
                    true,
                )?;
            }
            BotDispatchDisposition::Retryable => {
                return Err(SlackApiError::new(
                    "dispatch_retryable",
                    "Slack command dispatch failed before durable acceptance",
                ));
            }
        }
        return Ok(SlackMessageReport {
            delivered: true,
            command_replied: true,
        });
    }
    heartbeat_owned(store, owner_id)?;
    relay_inbound(store, runtime, envelope, from, text, timestamp)?;
    complete_staged_position(store, owner_id, envelope, timestamp, Some(timestamp), true)?;
    Ok(SlackMessageReport {
        delivered: true,
        command_replied: false,
    })
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SlackIterationReport {
    inbound_delivered: usize,
    outbound_delivered: usize,
    pages: usize,
}

fn run_iteration<A, D>(
    store: &dyn Store,
    api: &mut A,
    runtime: &SlackBridgeRuntimeConfig,
    owner_id: &str,
    envelope: &mut BridgeCursorEnvelope,
    page_budget: usize,
    dispatch: &mut D,
) -> SlackApiResult<SlackIterationReport>
where
    A: SlackApi,
    D: FnMut(&BotCommand, Option<&str>) -> BotDispatchOutcome,
{
    // Fence before any external or mesh side effect. A process that lost ownership
    // must stop before it can post or consume a row.
    heartbeat_owned(store, owner_id)?;
    prepare_staging_owned(store, owner_id, envelope)?;

    let mut report = SlackIterationReport::default();
    let mut command_replied = false;
    if envelope.position.is_empty() {
        let page = owned_history(
            store,
            owner_id,
            api,
            &SlackHistoryRequest {
                channel: &runtime.channel,
                oldest: "0",
                latest: None,
                limit: BOOTSTRAP_PAGE_SIZE,
            },
        )?;
        report.pages += 1;
        let position = match latest_ts(&page.messages) {
            Some(timestamp) => timestamp,
            None if page.messages.is_empty() => "0".to_string(),
            None => {
                return Err(SlackApiError::new(
                    "invalid_response",
                    "Slack bootstrap returned an invalid timestamp",
                ));
            }
        };
        advance_position(store, owner_id, envelope, &position, false)?;
    } else {
        let _budget = page_budget.clamp(1, MAX_HISTORY_PAGES_PER_ITERATION);
        // A route with no continuation and staged rows has already reached the
        // globally-oldest page. Drain that durable backlog before polling newer
        // history. Otherwise fetch exactly one tier-safe page and atomically stage
        // it with the next time bound, so restarts never need to rewalk prior pages.
        let staged_ready =
            envelope.continuation.is_none() && peek_staged_event(store, envelope)?.is_some();
        if !staged_ready {
            let requested_latest = envelope.continuation.clone();
            let page = owned_history(
                store,
                owner_id,
                api,
                &SlackHistoryRequest {
                    channel: &runtime.channel,
                    oldest: &envelope.position,
                    latest: requested_latest.as_deref(),
                    limit: HISTORY_PAGE_SIZE,
                },
            )?;
            report.pages += 1;

            let ordered = page
                .messages
                .iter()
                .map(|message| {
                    let raw = message.get("ts").and_then(Value::as_str).ok_or_else(|| {
                        SlackApiError::new(
                            "invalid_response",
                            "Slack history returned an invalid timestamp",
                        )
                    })?;
                    let timestamp = SlackTs::parse(raw).ok_or_else(|| {
                        SlackApiError::new(
                            "invalid_response",
                            "Slack history returned an invalid timestamp",
                        )
                    })?;
                    Ok((timestamp, staged_event(message)?))
                })
                .collect::<SlackApiResult<Vec<_>>>()?;

            let requested_latest_ts = match requested_latest.as_deref() {
                Some(raw) => Some(SlackTs::parse(raw).ok_or_else(|| {
                    SlackApiError::new("cursor", "Slack history time bound is invalid")
                })?),
                None => None,
            };
            if requested_latest_ts
                .as_ref()
                .is_some_and(|latest| ordered.iter().any(|(timestamp, _)| timestamp >= latest))
            {
                return Err(SlackApiError::new(
                    "invalid_response",
                    "Slack history crossed its latest time bound",
                ));
            }
            if page.has_more {
                let next_bound = ordered
                    .iter()
                    .map(|(timestamp, _)| timestamp)
                    .min()
                    .ok_or_else(|| {
                        SlackApiError::new(
                            "invalid_response",
                            "Slack history claimed more pages after an empty page",
                        )
                    })?;
                let above_watermark = envelope.position == "0"
                    || SlackTs::parse(&envelope.position)
                        .is_some_and(|position| next_bound > &position);
                let moved_older = requested_latest_ts
                    .as_ref()
                    .is_none_or(|latest| next_bound < latest);
                if !above_watermark || !moved_older {
                    return Err(SlackApiError::new(
                        "invalid_response",
                        "Slack history time bound did not move strictly older",
                    ));
                }
                let mut next = envelope.clone();
                next.continuation = Some(next_bound.raw.clone());
                let events = ordered
                    .into_iter()
                    .map(|(_, event)| event)
                    .collect::<Vec<_>>();
                stage_history_events(store, owner_id, envelope, next, &events)?;
            } else {
                let mut next = envelope.clone();
                next.continuation = None;
                let events = ordered
                    .into_iter()
                    .map(|(_, event)| event)
                    .collect::<Vec<_>>();
                stage_history_events(store, owner_id, envelope, next, &events)?;
            }
        }

        if envelope.continuation.is_none() {
            for _ in 0..MAX_STAGED_DRAIN_PER_ITERATION {
                let Some(event) = peek_staged_event(store, envelope)? else {
                    break;
                };
                let timestamp = SlackTs::parse(&event.position).ok_or_else(|| {
                    SlackApiError::new("store", "Slack staged event timestamp is invalid")
                })?;
                let already_seen = envelope.position != "0"
                    && SlackTs::parse(&envelope.position).is_some_and(|seen| timestamp <= seen);
                if already_seen {
                    complete_staged_position(
                        store,
                        owner_id,
                        envelope,
                        &event.position,
                        None,
                        false,
                    )?;
                    continue;
                }
                let handled = handle_history_message(
                    store, api, runtime, owner_id, envelope, &event, dispatch,
                )?;
                report.inbound_delivered += usize::from(handled.delivered);
                command_replied |= handled.command_replied;
                if command_replied {
                    break;
                }
            }
        }
    }

    // A command response already consumed this iteration's external delivery.
    // In particular, `/inbox` must not also drain an adjacent unread row.
    if command_replied {
        return Ok(report);
    }

    let mut retry_after = None;
    heartbeat_owned(store, owner_id)?;
    let outbound = crate::bridge::relay_outbound_once(
        store,
        BridgePlatform::Slack,
        &runtime.identity,
        MAX_OUTBOUND_POSTS_PER_ITERATION,
        |text| match owned_post(store, owner_id, api, &runtime.channel, text) {
            Ok(()) => Ok(()),
            Err(error) => {
                retry_after = error.retry_after_secs;
                Err(error.into_bridge())
            }
        },
    )
    .map_err(|_| SlackApiError::new("store", "Slack outbound relay failed locally"))?;
    report.outbound_delivered = outbound.delivered;
    if let Some(class) = outbound.error_class {
        return Err(SlackApiError {
            class,
            message: outbound
                .error
                .unwrap_or_else(|| "Slack outbound post failed".to_string()),
            retry_after_secs: retry_after,
        });
    }
    if outbound.delivered > 0 {
        persist_success(store, owner_id, envelope, true)?;
    }
    Ok(report)
}

/// Run the Slack bridge on the calling thread. Transient failures are classified,
/// persisted without provider detail, and retried; loss of the fenced owner stops
/// the old process before another side effect.
pub fn run(store: &dyn Store, config: &Config) -> Result<()> {
    let runtime = config.slack_bridge_runtime()?;
    if runtime.identity == runtime.recipient {
        anyhow::bail!("Slack bridge identity and recipient must be different");
    }
    let mut api = ReqwestSlackApi::new(runtime.token.clone())?;
    let owner_id = model::new_attempt_id(model::now());
    let owner_host = weave_core::config::this_host();
    let stale_before = model::now().saturating_sub(model::BRIDGE_ACTIVE_TTL_SECS);
    let claimed = store.claim_bridge_runtime(
        BridgePlatform::Slack,
        &runtime.identity,
        &runtime.recipient,
        &owner_id,
        Some(i64::from(std::process::id())),
        &owner_host,
        stale_before,
    )?;
    let Some(claimed) = claimed else {
        anyhow::bail!("Slack bridge is already active");
    };
    eprintln!(
        "[weave-slack] bridge started as identity '{}'",
        runtime.identity
    );

    let injector = crate::RealInjector {
        preferred_mux: crate::parse_mux_preference(config),
    };
    let result = (|| -> Result<()> {
        let auth = loop {
            heartbeat_owned(store, &owner_id)?;
            match api.auth_test() {
                Ok(auth) => break auth,
                Err(error) if error.class == "ownership_lost" => return Err(error.into()),
                Err(error) => {
                    persist_failure(store, &owner_id, &error)?;
                    eprintln!("[weave-slack] {}: {}", error.class, error.message);
                    let delay = error.retry_after_secs.unwrap_or(POLL_SECS);
                    wait_owned(store, &owner_id, Duration::from_secs(delay))?;
                }
            }
        };
        let mut envelope = load_envelope(&claimed.cursor, &auth, &runtime.channel)?;
        loop {
            let mut dispatch = |command: &BotCommand, idempotency_key: Option<&str>| {
                dispatch_bot_command_with_key(
                    store,
                    config,
                    &runtime.identity,
                    command,
                    &injector,
                    idempotency_key,
                )
            };
            match run_iteration(
                store,
                &mut api,
                &runtime,
                &owner_id,
                &mut envelope,
                MAX_HISTORY_PAGES_PER_ITERATION,
                &mut dispatch,
            ) {
                Ok(_) => wait_owned(store, &owner_id, Duration::from_secs(POLL_SECS))?,
                Err(error) if error.class == "ownership_lost" => {
                    anyhow::bail!("Slack bridge runtime ownership was lost")
                }
                Err(error) => {
                    persist_failure(store, &owner_id, &error)?;
                    eprintln!("[weave-slack] {}: {}", error.class, error.message);
                    let delay = error.retry_after_secs.unwrap_or(POLL_SECS);
                    wait_owned(store, &owner_id, Duration::from_secs(delay))?;
                }
            }
        }
    })();
    let release = store.release_bridge_runtime(BridgePlatform::Slack, &owner_id);
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(true)) => Ok(()),
        (Ok(()), Ok(false)) => anyhow::bail!("Slack bridge runtime ownership was lost"),
        (Ok(()), Err(error)) => Err(error.context("releasing Slack bridge runtime")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_DB: AtomicU64 = AtomicU64::new(1);

    struct TempDb {
        path: PathBuf,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-shm", "-wal"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    struct TestStore {
        // Fields drop in declaration order: close the backend before cleanup.
        store: Box<dyn Store>,
        _cleanup: TempDb,
    }

    fn test_store() -> TestStore {
        let n = NEXT_DB.fetch_add(1, AtomicOrdering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("weave-slack-runtime-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        #[cfg(feature = "sqlite")]
        let store: Box<dyn Store> =
            Box::new(weave_core::store::SqliteStore::open(&path).expect("sqlite Slack test store"));
        #[cfg(all(feature = "libsql", not(feature = "sqlite")))]
        let store: Box<dyn Store> = Box::new(
            weave_core::store_libsql::LibsqlStore::open(&Config {
                backend: Some("libsql".to_string()),
                db: Some(path.to_string_lossy().into_owned()),
                ..Config::default()
            })
            .expect("libSQL Slack test store"),
        );
        TestStore {
            store,
            _cleanup: TempDb { path },
        }
    }

    fn runtime() -> SlackBridgeRuntimeConfig {
        SlackBridgeRuntimeConfig {
            token: "test-token".to_string(),
            channel: "C123".to_string(),
            identity: "slack-bridge".to_string(),
            recipient: "agent".to_string(),
        }
    }

    fn auth() -> SlackAuth {
        SlackAuth {
            external_identity: "T1:U1".to_string(),
        }
    }

    fn envelope(position: &str) -> BridgeCursorEnvelope {
        BridgeCursorEnvelope {
            external_identity: auth().external_identity,
            external_scope: runtime().channel,
            position: position.to_string(),
            continuation: None,
        }
    }

    fn claim(store: &dyn Store, cursor: Option<&BridgeCursorEnvelope>) -> String {
        let owner = format!(
            "test-owner-{}",
            NEXT_DB.fetch_add(1, AtomicOrdering::Relaxed)
        );
        store
            .claim_bridge_runtime(
                BridgePlatform::Slack,
                "slack-bridge",
                "agent",
                &owner,
                Some(i64::from(std::process::id())),
                "test-host",
                0,
            )
            .unwrap()
            .unwrap();
        if let Some(cursor) = cursor {
            store
                .update_bridge_runtime(
                    BridgePlatform::Slack,
                    &owner,
                    &BridgeRuntimeUpdate {
                        cursor: Some(cursor.encode().unwrap()),
                        ..BridgeRuntimeUpdate::default()
                    },
                )
                .unwrap();
        }
        owner
    }

    fn restart_claim(store: &dyn Store, owner: &str) -> (String, BridgeCursorEnvelope) {
        assert!(store
            .release_bridge_runtime(BridgePlatform::Slack, owner)
            .unwrap());
        let replacement = claim(store, None);
        let stored = store
            .bridge_runtime_status(BridgePlatform::Slack)
            .unwrap()
            .unwrap();
        let cursor = load_envelope(&stored.cursor, &auth(), "C123").unwrap();
        (replacement, cursor)
    }

    #[derive(Default)]
    struct FakeApi {
        auth: VecDeque<SlackApiResult<SlackAuth>>,
        history: VecDeque<SlackApiResult<SlackHistoryPage>>,
        posts: Vec<String>,
        post_results: VecDeque<SlackApiResult<()>>,
        history_channels: Vec<String>,
        history_requests: Vec<(String, Option<String>, u16)>,
        calls: Vec<&'static str>,
    }

    impl SlackApi for FakeApi {
        fn auth_test(&mut self) -> SlackApiResult<SlackAuth> {
            self.calls.push("auth.test");
            self.auth
                .pop_front()
                .unwrap_or_else(|| Ok(super::tests::auth()))
        }

        fn history(
            &mut self,
            request: &SlackHistoryRequest<'_>,
        ) -> SlackApiResult<SlackHistoryPage> {
            self.calls.push("conversations.history");
            self.history_channels.push(request.channel.to_string());
            self.history_requests.push((
                request.oldest.to_string(),
                request.latest.map(str::to_string),
                request.limit,
            ));
            self.history.pop_front().unwrap_or_else(|| {
                Ok(SlackHistoryPage {
                    messages: Vec::new(),
                    has_more: false,
                })
            })
        }

        fn post_message(&mut self, _channel: &str, text: &str) -> SlackApiResult<()> {
            self.calls.push("chat.postMessage");
            self.posts.push(text.to_string());
            self.post_results.pop_front().unwrap_or(Ok(()))
        }
    }

    struct LeaseStealingApi<'a> {
        store: &'a dyn Store,
        owner_id: String,
        history_calls: usize,
        posts: usize,
    }

    impl SlackApi for LeaseStealingApi<'_> {
        fn auth_test(&mut self) -> SlackApiResult<SlackAuth> {
            Ok(auth())
        }

        fn history(
            &mut self,
            _request: &SlackHistoryRequest<'_>,
        ) -> SlackApiResult<SlackHistoryPage> {
            self.history_calls += 1;
            assert!(self
                .store
                .release_bridge_runtime(BridgePlatform::Slack, &self.owner_id)
                .unwrap());
            self.store
                .claim_bridge_runtime(
                    BridgePlatform::Slack,
                    "slack-bridge",
                    "agent",
                    "replacement-owner",
                    None,
                    "test-host",
                    0,
                )
                .unwrap()
                .unwrap();
            Ok(page(
                vec![json!({"ts":"101.000000","user":"U1","text":"do not relay"})],
                false,
            ))
        }

        fn post_message(&mut self, _channel: &str, _text: &str) -> SlackApiResult<()> {
            self.posts += 1;
            Ok(())
        }
    }

    struct PostLeaseStealingApi<'a> {
        store: &'a dyn Store,
        owner_id: String,
        posts: usize,
    }

    impl SlackApi for PostLeaseStealingApi<'_> {
        fn auth_test(&mut self) -> SlackApiResult<SlackAuth> {
            Ok(auth())
        }

        fn history(
            &mut self,
            _request: &SlackHistoryRequest<'_>,
        ) -> SlackApiResult<SlackHistoryPage> {
            Ok(page(Vec::new(), false))
        }

        fn post_message(&mut self, _channel: &str, _text: &str) -> SlackApiResult<()> {
            self.posts += 1;
            assert!(self
                .store
                .release_bridge_runtime(BridgePlatform::Slack, &self.owner_id)
                .unwrap());
            self.store
                .claim_bridge_runtime(
                    BridgePlatform::Slack,
                    "slack-bridge",
                    "agent",
                    "post-replacement-owner",
                    None,
                    "test-host",
                    0,
                )
                .unwrap()
                .unwrap();
            Ok(())
        }
    }

    fn page(messages: Vec<Value>, has_more: bool) -> SlackHistoryPage {
        SlackHistoryPage { messages, has_more }
    }

    fn no_command(_: &BotCommand, _: Option<&str>) -> BotDispatchOutcome {
        BotDispatchOutcome::terminal("ok")
    }

    #[test]
    fn payload_shapes_are_explicit_and_exclusive() {
        let post = slack_post_payload("C123", "hello");
        assert_eq!(post["channel"], "C123");
        assert_eq!(post["text"], "hello");
        let history = slack_history_payload("C123", "1700000000.000100", Some("1700000001.000200"));
        assert_eq!(history["oldest"], "1700000000.000100");
        assert_eq!(history["latest"], "1700000001.000200");
        assert_eq!(history["inclusive"], false);
        assert_eq!(history["limit"], HISTORY_PAGE_SIZE);
    }

    #[test]
    fn history_safe_command_parser_covers_every_argument_shape() {
        assert_eq!(parse_slack_command("!weave"), Some(BotCommand::Help));
        assert_eq!(parse_slack_command("!weave inbox"), Some(BotCommand::Inbox));
        assert_eq!(parse_slack_command("!weave peers"), Some(BotCommand::Peers));
        assert_eq!(
            parse_slack_command("!weave sessions"),
            Some(BotCommand::Sessions)
        );
        assert_eq!(
            parse_slack_command("!weave send agent hello there"),
            Some(BotCommand::Send {
                to: "agent".to_string(),
                body: "hello there".to_string(),
            })
        );
        assert_eq!(
            parse_slack_command("!weave ask agent ready?"),
            Some(BotCommand::Ask {
                to: "agent".to_string(),
                body: "ready?".to_string(),
            })
        );
        assert_eq!(
            parse_slack_command("!weave answer ask_1 yes"),
            Some(BotCommand::Answer {
                id: "ask_1".to_string(),
                body: "yes".to_string(),
            })
        );
        assert_eq!(
            parse_slack_command("!weave reply 42 ack"),
            Some(BotCommand::Reply {
                message_id: 42,
                body: "ack".to_string(),
            })
        );
        assert_eq!(parse_slack_command("!weave help"), Some(BotCommand::Help));
        assert_eq!(parse_slack_command("/inbox"), Some(BotCommand::Inbox));
        assert_eq!(parse_slack_command("!ordinary text"), None);
        assert_eq!(parse_slack_command("!weaver inbox"), None);
        assert_eq!(
            parse_slack_command("!weave send agent"),
            Some(BotCommand::Help)
        );
    }

    #[test]
    fn check_requires_configured_channel_read_without_posting() {
        let mut success = FakeApi::default();
        let checked = check_with_api(&runtime(), &mut success).unwrap();
        assert_eq!(checked.external_identity.as_deref(), Some("T1:U1"));
        assert_eq!(checked.external_scope.as_deref(), Some("C123"));
        assert_eq!(success.calls, ["auth.test", "conversations.history"]);
        assert_eq!(success.history_channels, vec!["C123"]);
        assert_eq!(
            success.history_requests,
            vec![("0".to_string(), None, BOOTSTRAP_PAGE_SIZE)]
        );
        assert!(success.posts.is_empty());

        let mut denied = FakeApi {
            history: VecDeque::from([Err(SlackApiError::new(
                "api_rejected",
                "TOKEN_OR_CHANNEL_SENTINEL",
            ))]),
            ..FakeApi::default()
        };
        let error = check_with_api(&runtime(), &mut denied).unwrap_err();
        assert_eq!(error.class, "channel_read");
        assert_eq!(error.message, "Slack configured channel could not be read");
        assert!(!format!("{error:?}").contains("TOKEN_OR_CHANNEL_SENTINEL"));
        assert_eq!(denied.calls, ["auth.test", "conversations.history"]);
        assert!(denied.posts.is_empty());
    }

    #[test]
    fn exact_timestamp_order_does_not_collapse_adjacent_microseconds() {
        let messages = vec![
            json!({"ts":"1700000000.000001"}),
            json!({"ts":"1700000000.000002"}),
            json!({"ts":"1699999999.999999"}),
        ];
        assert_eq!(latest_ts(&messages).as_deref(), Some("1700000000.000002"));
        assert!(
            SlackTs::parse("1700000000.000001").unwrap()
                < SlackTs::parse("1700000000.000002").unwrap()
        );
        assert_eq!(
            SlackTs::parse("1.1").unwrap(),
            SlackTs::parse("01.10").unwrap()
        );
        assert!(SlackTs::parse("1e3").is_none());
    }

    #[test]
    fn response_validation_requires_ok_and_bounds_and_classifies_rate_limit() {
        assert!(validate_slack_bytes(200, br#"{"ok":true}"#, None).is_ok());
        assert_eq!(
            validate_slack_bytes(200, br#"{"ok":false,"error":"bad_auth"}"#, None)
                .unwrap_err()
                .class,
            "api_rejected"
        );
        let oversized = vec![b'x'; crate::bridge::MAX_BRIDGE_RESPONSE_BYTES + 1];
        assert_eq!(
            validate_slack_bytes(200, &oversized, None)
                .unwrap_err()
                .class,
            "response_too_large"
        );
        let limited = validate_slack_bytes(429, b"", Some(2)).unwrap_err();
        assert_eq!(limited.class, "rate_limited");
        assert_eq!(limited.retry_after_secs, Some(POLL_SECS));
    }

    #[test]
    fn auth_and_history_reject_unbounded_or_malformed_fields() {
        assert!(parse_auth(&json!({"user_id":"U1\n","team_id":"T1"})).is_err());
        assert!(parse_auth(&json!({
            "user_id": "U1",
            "team_id": "x".repeat(model::MAX_BRIDGE_ROUTE_FIELD_LEN + 1)
        }))
        .is_err());
        let page = parse_history_page(
            &json!({"messages": [{"ts":"1.000001"}], "response_metadata": {
                "next_cursor": "bad\ncursor"
            }, "has_more": false}),
            50,
        )
        .unwrap();
        assert!(!page.has_more, "opaque cursors are ignored");
        assert!(parse_history_page(&json!({"messages": []}), 15).is_err());
        let error = parse_history_page(&json!({"messages": [], "has_more": true}), 15).unwrap_err();
        assert_eq!(error.class, "invalid_response");
        assert_eq!(
            error.message,
            "Slack history claimed more pages after an empty page"
        );
    }

    #[test]
    fn valid_bootstrap_skips_backlog_and_persists_exact_route() {
        let test = test_store();
        let owner = claim(test.store.as_ref(), None);
        let mut cursor = new_envelope(&auth(), "C123");
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![json!({"ts":"99.000001","user":"U1","text":"old"})],
                true,
            ))]),
            ..FakeApi::default()
        };
        let report = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(report.inbound_delivered, 0);
        assert_eq!(cursor.position, "99.000001");
        assert!(cursor.continuation.is_none());
        assert!(cursor.route_matches("T1:U1", "C123"));
        assert_eq!(
            test.store.inbox("agent", false, false, 10).unwrap().0.len(),
            0
        );
    }

    #[test]
    fn route_change_discards_incompatible_position_and_bootstraps_again() {
        let old = BridgeCursorEnvelope {
            external_identity: "T-old:U-old".to_string(),
            external_scope: "C-old".to_string(),
            position: "999.000001".to_string(),
            continuation: Some("old-page".to_string()),
        };
        let reset = load_envelope(&old.encode().unwrap(), &auth(), "C123").unwrap();
        assert!(reset.position.is_empty());
        assert!(reset.continuation.is_none());
        assert!(reset.route_matches("T1:U1", "C123"));
    }

    #[test]
    fn load_discards_legacy_or_non_advancing_continuation_only() {
        let mut legacy = envelope("100.000000");
        legacy.continuation = Some("opaque-page-2".to_string());
        let loaded = load_envelope(&legacy.encode().unwrap(), &auth(), "C123").unwrap();
        assert_eq!(loaded.position, "100.000000");
        assert!(loaded.continuation.is_none());

        let mut repeated = envelope("100.000000");
        repeated.continuation = Some("100.000000".to_string());
        let loaded = load_envelope(&repeated.encode().unwrap(), &auth(), "C123").unwrap();
        assert_eq!(loaded.position, "100.000000");
        assert!(loaded.continuation.is_none());

        let mut valid = envelope("100.000000");
        valid.continuation = Some("120.000000".to_string());
        let loaded = load_envelope(&valid.encode().unwrap(), &auth(), "C123").unwrap();
        assert_eq!(loaded.continuation.as_deref(), Some("120.000000"));
    }

    #[test]
    fn event_key_binds_checked_account_channel_and_exact_timestamp() {
        let base = envelope("100.000000");
        let key = inbound_idempotency_key(&base, "101.000001");
        let mut other_account = base.clone();
        other_account.external_identity = "T2:U2".to_string();
        let mut other_channel = base.clone();
        other_channel.external_scope = "C999".to_string();
        assert_ne!(key, inbound_idempotency_key(&other_account, "101.000001"));
        assert_ne!(key, inbound_idempotency_key(&other_channel, "101.000001"));
        assert_ne!(key, inbound_idempotency_key(&base, "101.000002"));
        assert!(weave_core::model::idempotency_key_valid(&key));
    }

    #[test]
    fn staged_pagination_is_linear_and_restart_safe_across_four_pages() {
        let test = test_store();
        let initial = envelope("100.000000");
        let mut owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut first = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"180.000000","user":"U8","text":"event-8"}),
                    json!({"ts":"170.000000","user":"U7","text":"event-7"}),
                ],
                true,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut first,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(cursor.position, "100.000000");
        assert_eq!(cursor.continuation.as_deref(), Some("170.000000"));
        assert_eq!(first.history_requests[0].0, "100.000000");
        assert!(first.history_requests[0].1.is_none());
        assert!(test
            .store
            .inbox("agent", false, false, 10)
            .unwrap()
            .0
            .is_empty());

        // Every page boundary survives a real owner release/reclaim. Each new
        // worker starts from the durable continuation instead of refetching page 1.
        (owner, cursor) = restart_claim(test.store.as_ref(), &owner);
        let mut second = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"160.000000","user":"U6","text":"event-6"}),
                    json!({"ts":"150.000000","user":"U5","text":"event-5"}),
                ],
                true,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut second,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(second.history_requests[0].0, "100.000000");
        assert_eq!(second.history_requests[0].1.as_deref(), Some("170.000000"));
        assert_eq!(cursor.position, "100.000000");
        assert_eq!(cursor.continuation.as_deref(), Some("150.000000"));

        (owner, cursor) = restart_claim(test.store.as_ref(), &owner);
        let mut third = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"140.000000","user":"U4","text":"event-4"}),
                    json!({"ts":"130.000000","user":"U3","text":"event-3"}),
                ],
                true,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut third,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(third.history_requests[0].0, "100.000000");
        assert_eq!(third.history_requests[0].1.as_deref(), Some("150.000000"));
        assert_eq!(cursor.position, "100.000000");
        assert_eq!(cursor.continuation.as_deref(), Some("130.000000"));

        (owner, cursor) = restart_claim(test.store.as_ref(), &owner);
        let mut fourth = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"120.000000","user":"U2","text":"event-2"}),
                    json!({"ts":"110.000000","user":"U1","text":"event-1"}),
                ],
                false,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut fourth,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(fourth.history_requests[0].0, "100.000000");
        assert_eq!(fourth.history_requests[0].1.as_deref(), Some("130.000000"));
        let rows = test.store.inbox("agent", false, false, 10).unwrap().0;
        assert_eq!(rows.len(), 8);
        for (index, row) in rows.iter().enumerate() {
            assert!(
                row.body.contains(&format!("event-{}", index + 1)),
                "globally oldest-first row {index}: {}",
                row.body
            );
        }
        assert_eq!(cursor.position, "180.000000");
        assert!(cursor.continuation.is_none());

        // Four provider pages required exactly four calls, not 4+3+2+1 refetches.
        let history_calls = first.history_requests.len()
            + second.history_requests.len()
            + third.history_requests.len()
            + fourth.history_requests.len();
        assert_eq!(history_calls, 4);
        assert!(peek_staged_event(test.store.as_ref(), &cursor)
            .unwrap()
            .is_none());
    }

    #[test]
    fn timestamp_pagination_bound_cannot_repeat_or_cycle_newer() {
        for invalid_bound in ["120.000000", "130.000000"] {
            let test = test_store();
            let mut initial = envelope("100.000000");
            initial.continuation = Some("120.000000".to_string());
            let owner = claim(test.store.as_ref(), Some(&initial));
            let mut cursor = initial;
            let mut api = FakeApi {
                history: VecDeque::from([Ok(page(
                    vec![json!({"ts":invalid_bound,"user":"U1","text":"bad-bound"})],
                    true,
                ))]),
                ..FakeApi::default()
            };
            let error = run_iteration(
                test.store.as_ref(),
                &mut api,
                &runtime(),
                &owner,
                &mut cursor,
                1,
                &mut no_command,
            )
            .unwrap_err();
            assert_eq!(error.class, "invalid_response");
            assert_eq!(cursor.position, "100.000000");
            assert_eq!(cursor.continuation.as_deref(), Some("120.000000"));
        }
    }

    #[test]
    fn paginated_page_rejects_any_event_crossing_latest_bound() {
        let test = test_store();
        let mut initial = envelope("100.000000");
        initial.continuation = Some("120.000000".to_string());
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"120.000000","user":"U2","text":"crosses-bound"}),
                    json!({"ts":"110.000000","user":"U1","text":"older-minimum"}),
                ],
                true,
            ))]),
            ..FakeApi::default()
        };
        let error = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap_err();
        assert_eq!(error.class, "invalid_response");
        assert_eq!(error.message, "Slack history crossed its latest time bound");
        assert_eq!(cursor.position, "100.000000");
        assert_eq!(cursor.continuation.as_deref(), Some("120.000000"));
        assert!(peek_staged_event(test.store.as_ref(), &cursor)
            .unwrap()
            .is_none());
    }

    #[test]
    fn production_page_budget_never_bursts_the_one_per_minute_history_tier() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([
                Ok(page(
                    vec![json!({"ts":"120.000000","user":"U2","text":"new"})],
                    true,
                )),
                Ok(page(
                    vec![json!({"ts":"110.000000","user":"U1","text":"old"})],
                    false,
                )),
            ]),
            ..FakeApi::default()
        };
        let report = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            usize::MAX,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(report.pages, 1);
        assert_eq!(api.history_requests.len(), 1);
        assert_eq!(api.history_requests[0].2, 15);
        assert!(api.history_requests[0].1.is_none());
        assert_eq!(cursor.continuation.as_deref(), Some("120.000000"));
        assert_eq!(api.history.len(), 1);
    }

    #[test]
    fn replay_is_idempotent_and_uses_bridge_identity_to_recipient() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let events = [
            json!({"ts":"101.000001","user":"U-human","text":"hello"}),
            json!({"ts":"101.000001","user":"U-human","text":"edited later"}),
        ];
        for event in events {
            let mut replay_cursor = initial.clone();
            let mut api = FakeApi {
                history: VecDeque::from([Ok(page(vec![event], false))]),
                ..FakeApi::default()
            };
            run_iteration(
                test.store.as_ref(),
                &mut api,
                &runtime(),
                &owner,
                &mut replay_cursor,
                1,
                &mut no_command,
            )
            .unwrap();
        }
        let rows = test.store.inbox("agent", false, false, 10).unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "slack-bridge");
        assert_eq!(rows[0].recipient, "agent");
        assert!(rows[0].body.contains("U-human"));
        assert!(rows[0].body.contains("hello"));
        assert!(!rows[0].body.contains("edited later"));
    }

    #[test]
    fn edited_ordinary_replay_accepts_first_command_shaped_message() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let event_key = inbound_idempotency_key(&initial, "101.000001");
        test.store
            .send_idempotent(
                "agent",
                "different-recipient",
                Some("command result"),
                "first accepted representation",
                Some(&event_key),
                None,
            )
            .unwrap();

        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![json!({
                    "ts":"101.000001",
                    "user":"U-human",
                    "text":"edited ordinary representation"
                })],
                false,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();

        assert_eq!(cursor.position, "101.000001");
        assert!(test
            .store
            .inbox("agent", false, false, 10)
            .unwrap()
            .0
            .is_empty());
        assert_eq!(
            test.store
                .message_by_idempotency_key(&event_key)
                .unwrap()
                .unwrap()
                .body,
            "first accepted representation"
        );
    }

    #[test]
    fn ordinary_bang_text_relay_is_not_interpreted_as_a_command() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![json!({"ts":"101.000001","user":"U-human","text":"!ordinary text"})],
                false,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        let rows = test.store.inbox("agent", false, false, 10).unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "[Slack U-human] !ordinary text");
        assert!(api.posts.is_empty());
    }

    #[test]
    fn ignored_subtype_is_staged_drained_and_advances_before_human_event() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"102.000000","user":"U2","text":"human"}),
                    json!({"ts":"101.000000","subtype":"channel_join","text":"ignored"}),
                ],
                false,
            ))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(cursor.position, "102.000000");
        let rows = test.store.inbox("agent", false, false, 10).unwrap().0;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].body.contains("human"));
        assert!(peek_staged_event(test.store.as_ref(), &cursor)
            .unwrap()
            .is_none());
    }

    #[test]
    fn slack_help_advertises_history_safe_prefix() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![json!({"ts":"101.000001","user":"U1","text":"!weave help"})],
                false,
            ))]),
            ..FakeApi::default()
        };
        let mut must_not_dispatch = |_: &BotCommand, _: Option<&str>| -> BotDispatchOutcome {
            panic!("Slack help must be rendered locally")
        };
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut must_not_dispatch,
        )
        .unwrap();
        assert_eq!(api.posts.len(), 1);
        assert!(api.posts[0].contains("!weave inbox"));
        assert!(api.posts[0].contains("!weave ask <to> <body>"));
    }

    #[test]
    fn retryable_dispatch_retries_to_one_success_without_early_post_or_cursor() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let command = json!({"ts":"101.000001","user":"U1","text":"!weave send agent once"});
        let mut cursor = initial.clone();
        let calls = Cell::new(0);
        let mut dispatch = |parsed: &BotCommand, _: Option<&str>| {
            assert!(matches!(parsed, BotCommand::Send { .. }));
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                BotDispatchOutcome::retryable()
            } else {
                BotDispatchOutcome::durable("sent")
            }
        };
        let mut first = FakeApi {
            history: VecDeque::from([Ok(page(vec![command.clone()], false))]),
            ..FakeApi::default()
        };
        let error = run_iteration(
            test.store.as_ref(),
            &mut first,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap_err();
        assert_eq!(error.class, "dispatch_retryable");
        assert_eq!(cursor.position, "100.000000");
        assert!(first.posts.is_empty());

        let mut second = FakeApi {
            history: VecDeque::from([Ok(page(vec![command.clone()], false))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut second,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(cursor.position, "101.000001");
        assert_eq!(second.posts, vec!["sent"]);

        let mut consumed = FakeApi {
            history: VecDeque::from([Ok(page(vec![command], false))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut consumed,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(consumed.posts.is_empty());
    }

    #[test]
    fn permanent_unknown_commands_post_once_per_iteration_then_advance() {
        let _env = weave_core::testenv::lock_env();
        let _writes = weave_core::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = FakeApi {
            history: VecDeque::from([Ok(page(
                vec![
                    json!({"ts":"101.000001","user":"U1","text":"!weave reply 999 missing"}),
                    json!({"ts":"102.000001","user":"U1","text":"!weave answer ask_999_1 missing"}),
                ],
                false,
            ))]),
            ..FakeApi::default()
        };
        let injector = crate::RealInjector {
            preferred_mux: None,
        };
        let config = Config::default();
        let mut dispatch = |command: &BotCommand, key: Option<&str>| {
            dispatch_bot_command_with_key(
                test.store.as_ref(),
                &config,
                "slack-bridge",
                command,
                &injector,
                key,
            )
        };
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(cursor.position, "101.000001");
        assert_eq!(api.posts.len(), 1);
        run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(cursor.position, "102.000001");
        assert_eq!(api.posts.len(), 2);
        assert!(api.posts.iter().all(|reply| reply.contains("Error:")));
        assert_eq!(api.history_requests.len(), 1);
        assert!(test.store.all_messages(10).unwrap().is_empty());
    }

    #[test]
    fn malformed_oversized_and_future_cursors_fail_closed() {
        for stored in [
            "not-json".to_string(),
            "x".repeat(3_000),
            json!({
                "external_identity":"T1:U1",
                "external_scope":"C123",
                "position":"100.000000",
                "version":2
            })
            .to_string(),
        ] {
            let error = load_envelope(&stored, &auth(), "C123").unwrap_err();
            assert_eq!(error.class, "cursor");
        }
    }

    #[test]
    fn send_is_not_reexecuted_after_best_effort_reply_failure() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let command = json!({"ts":"101.000001","user":"U1","text":"/send agent hello"});
        let mut cursor = initial.clone();
        let mut keys = Vec::new();
        let mut dispatch = |parsed: &BotCommand, key: Option<&str>| {
            assert!(matches!(parsed, BotCommand::Send { .. }));
            keys.push(key.unwrap().to_string());
            BotDispatchOutcome::durable("sent")
        };
        let mut failed = FakeApi {
            history: VecDeque::from([Ok(page(vec![command.clone()], false))]),
            post_results: VecDeque::from([Err(SlackApiError::new(
                "api_rejected",
                "Slack API rejected the request",
            ))]),
            ..FakeApi::default()
        };
        assert!(run_iteration(
            test.store.as_ref(),
            &mut failed,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .is_err());
        assert_eq!(cursor.position, "101.000001");

        let mut succeeded = FakeApi {
            history: VecDeque::from([Ok(page(vec![command], false))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut succeeded,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], inbound_idempotency_key(&initial, "101.000001"));
        assert_eq!(cursor.position, "101.000001");
        assert!(succeeded.posts.is_empty());
    }

    #[test]
    fn mutating_command_is_not_reexecuted_after_reply_failure() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let command = json!({"ts":"101.000001","user":"U1","text":"/ask agent hello"});
        let mut cursor = initial;
        let calls = Cell::new(0);
        let mut dispatch = |parsed: &BotCommand, _key: Option<&str>| {
            assert!(matches!(parsed, BotCommand::Ask { .. }));
            calls.set(calls.get() + 1);
            BotDispatchOutcome::durable("asked")
        };
        let mut failed = FakeApi {
            history: VecDeque::from([Ok(page(vec![command.clone()], false))]),
            post_results: VecDeque::from([Err(SlackApiError::new(
                "api_rejected",
                "Slack API rejected the request",
            ))]),
            ..FakeApi::default()
        };
        assert!(run_iteration(
            test.store.as_ref(),
            &mut failed,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .is_err());
        assert_eq!(cursor.position, "101.000001");
        assert_eq!(calls.get(), 1);

        let mut replay = FakeApi {
            history: VecDeque::from([Ok(page(vec![command], false))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut replay,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(replay.posts.is_empty());
    }

    #[test]
    fn failed_inbox_reply_stays_unread_and_success_does_not_drain_adjacent_row() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        test.store
            .send("agent", "slack-bridge", None, "pending-one", None, None)
            .unwrap();
        test.store
            .send("agent", "slack-bridge", None, "pending-two", None, None)
            .unwrap();

        let command = json!({"ts":"101.000000","user":"U1","text":"!weave inbox"});
        let mut failed = FakeApi {
            history: VecDeque::from([Ok(page(vec![command.clone()], false))]),
            post_results: VecDeque::from([Err(SlackApiError::new(
                "api_rejected",
                "Slack API rejected the request",
            ))]),
            ..FakeApi::default()
        };
        let mut cursor = initial.clone();
        assert!(run_iteration(
            test.store.as_ref(),
            &mut failed,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .is_err());
        assert_eq!(test.store.unread_count("slack-bridge").unwrap(), 2);
        assert_eq!(cursor.position, "100.000000");

        let mut succeeded = FakeApi {
            history: VecDeque::from([Ok(page(vec![command], false))]),
            ..FakeApi::default()
        };
        run_iteration(
            test.store.as_ref(),
            &mut succeeded,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(test.store.unread_count("slack-bridge").unwrap(), 1);
        assert!(succeeded.posts[0].contains("pending-one"));
        assert!(
            succeeded.posts[0].ends_with("(1 more unread)"),
            "two-row snapshot footer must count only the undisplayed row: {}",
            succeeded.posts[0]
        );
        assert_eq!(
            test.store
                .peek_oldest_unread("slack-bridge")
                .unwrap()
                .unwrap()
                .body,
            "pending-two"
        );
    }

    #[test]
    fn ordinary_outbound_failure_stays_unread_until_provider_acceptance() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        test.store
            .send("agent", "slack-bridge", None, "pending", None, None)
            .unwrap();
        let mut cursor = initial;
        let mut failed = FakeApi {
            history: VecDeque::from([Ok(page(Vec::new(), false))]),
            post_results: VecDeque::from([Err(SlackApiError::rate_limited(Some(60)))]),
            ..FakeApi::default()
        };
        let error = run_iteration(
            test.store.as_ref(),
            &mut failed,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap_err();
        assert_eq!(error.class, "rate_limited");
        assert_eq!(error.retry_after_secs, Some(60));
        assert_eq!(test.store.unread_count("slack-bridge").unwrap(), 1);

        let mut succeeded = FakeApi {
            history: VecDeque::from([Ok(page(Vec::new(), false))]),
            ..FakeApi::default()
        };
        let report = run_iteration(
            test.store.as_ref(),
            &mut succeeded,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap();
        assert_eq!(report.outbound_delivered, 1);
        assert_eq!(test.store.unread_count("slack-bridge").unwrap(), 0);
        assert!(succeeded.posts[0].contains("pending"));
    }

    #[test]
    fn owner_fence_is_checked_before_api_or_delivery() {
        let test = test_store();
        let mut cursor = envelope("100.000000");
        claim(test.store.as_ref(), Some(&cursor));
        let mut api = FakeApi::default();
        let error = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            "wrong-owner",
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert!(api.history_requests.is_empty());
        assert!(api.posts.is_empty());
    }

    #[test]
    fn owner_is_refenced_after_history_before_any_side_effect() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        let mut cursor = initial;
        let mut api = LeaseStealingApi {
            store: test.store.as_ref(),
            owner_id: owner.clone(),
            history_calls: 0,
            posts: 0,
        };
        let error = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert_eq!(api.history_calls, 1);
        assert_eq!(api.posts, 0);
        assert_eq!(cursor.position, "100.000000");
        assert!(test
            .store
            .inbox("agent", false, false, 10)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn owner_is_refenced_after_post_before_local_acknowledgement() {
        let test = test_store();
        let initial = envelope("100.000000");
        let owner = claim(test.store.as_ref(), Some(&initial));
        test.store
            .send(
                "agent",
                "slack-bridge",
                None,
                "must-stay-unread",
                None,
                None,
            )
            .unwrap();
        let mut cursor = initial;
        let mut api = PostLeaseStealingApi {
            store: test.store.as_ref(),
            owner_id: owner.clone(),
            posts: 0,
        };
        let error = run_iteration(
            test.store.as_ref(),
            &mut api,
            &runtime(),
            &owner,
            &mut cursor,
            1,
            &mut no_command,
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert_eq!(api.posts, 1);
        assert_eq!(test.store.unread_count("slack-bridge").unwrap(), 1);
        assert_eq!(cursor.position, "100.000000");
    }
}
