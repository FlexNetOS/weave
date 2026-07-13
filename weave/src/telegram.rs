//! Telegram bridge (`weave telegram`).
//!
//! The production loop and the deterministic tests share the same single-poll
//! state machine. HTTP responses are bounded and validated before parsing, the
//! persisted cursor is fenced to the checked bot account + configured chat, and
//! external delivery is acknowledged only after Telegram accepts it.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use weave_core::config::{Config, TelegramBridgeRuntimeConfig};
use weave_core::model::{
    now, AskState, BridgeCursorEnvelope, BridgePlatform, BridgeRuntimeErrorUpdate,
    BridgeRuntimeStatus, BridgeRuntimeUpdate, Message, BRIDGE_ACTIVE_TTL_SECS,
    MAX_BRIDGE_ROUTE_FIELD_LEN,
};
use weave_core::store::{self, Store, MAX_BODY};

use crate::bridge::{
    decode_cursor_strict, relay_outbound_once, BridgeCallError, BridgeCheck, MAX_BRIDGE_BATCH,
    MAX_BRIDGE_POST_RESPONSE_BYTES, MAX_BRIDGE_RESPONSE_BYTES, MAX_BRIDGE_TEXT_CHARS,
};

const POLL_TIMEOUT_SECS: u64 = 25;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const RETRY_DELAY_SECS: u64 = 5;
const MAX_TELEGRAM_UPDATES: usize = 100;
const MAX_INBOX_SNAPSHOT: i64 = 50;

type CallResult<T> = std::result::Result<T, BridgeCallError>;

/// Build the JSON body for Telegram `sendMessage`.
pub fn telegram_send_payload(chat_id: &str, text: &str) -> Value {
    json!({ "chat_id": chat_id, "text": text })
}

/// Build the JSON body for `getUpdates`. Runtime cursors store the last handled
/// update id, so normal callers pass `last_update_id + 1`. A bootstrap uses `-1`
/// to discard an old pending backlog and establish a validated current boundary.
pub fn telegram_get_updates_payload(offset: i64, timeout_secs: u64) -> Value {
    json!({ "offset": offset, "timeout": timeout_secs })
}

/// Parse one text update into its display sender and body. This compatibility
/// helper intentionally does not apply the runtime chat allowlist.
#[allow(dead_code)]
pub fn parse_telegram_update(update: &Value) -> Option<(String, String)> {
    let message = update.get("message")?;
    if message
        .pointer("/from/is_bot")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let text = message.get("text")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let from = telegram_sender(message);
    Some((from, text.to_string()))
}

/// Bound an untrusted external handle to the weave identity-safe subset. Runtime
/// inbound messages are sent *from the configured bridge identity*; this value is
/// used only as bounded attribution inside the body.
pub fn sanitize_inbound_ident(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Render a reqwest failure without its credential-bearing URL.
pub fn redact_reqwest_error(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect failed"
    } else if error.is_decode() {
        "decode failed"
    } else if error.is_request() {
        "request failed"
    } else {
        "http error"
    };
    match error.status() {
        Some(status) => format!("{kind} (status {})", status.as_u16()),
        None => kind.to_string(),
    }
}

fn reqwest_error(error: reqwest::Error) -> BridgeCallError {
    BridgeCallError::new("transport", redact_reqwest_error(&error))
}

fn local_error(class: &str, message: &'static str) -> BridgeCallError {
    BridgeCallError::new(class, message)
}

trait TelegramTransport {
    fn get_me(&mut self) -> CallResult<Value>;
    fn get_chat(&mut self, chat_id: &str) -> CallResult<Value>;
    fn get_updates(&mut self, offset: i64, timeout_secs: u64) -> CallResult<Value>;
    fn send_message(&mut self, chat_id: &str, text: &str) -> CallResult<()>;
}

struct HttpTelegramTransport {
    client: reqwest::blocking::Client,
    base: String,
}

impl HttpTelegramTransport {
    fn new(token: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .context("building Telegram HTTP client")?;
        Ok(Self {
            client,
            base: format!("https://api.telegram.org/bot{token}"),
        })
    }

    fn read(
        response: std::result::Result<reqwest::blocking::Response, reqwest::Error>,
        max_bytes: usize,
    ) -> CallResult<Value> {
        let response = response.map_err(reqwest_error)?;
        crate::bridge::read_api_response(response, max_bytes)
    }
}

impl TelegramTransport for HttpTelegramTransport {
    fn get_me(&mut self) -> CallResult<Value> {
        Self::read(
            self.client.post(format!("{}/getMe", self.base)).send(),
            MAX_BRIDGE_POST_RESPONSE_BYTES,
        )
    }

    fn get_chat(&mut self, chat_id: &str) -> CallResult<Value> {
        Self::read(
            self.client
                .post(format!("{}/getChat", self.base))
                .json(&json!({"chat_id": chat_id}))
                .send(),
            MAX_BRIDGE_POST_RESPONSE_BYTES,
        )
    }

    fn get_updates(&mut self, offset: i64, timeout_secs: u64) -> CallResult<Value> {
        Self::read(
            self.client
                .post(format!("{}/getUpdates", self.base))
                .json(&telegram_get_updates_payload(offset, timeout_secs))
                .send(),
            MAX_BRIDGE_RESPONSE_BYTES,
        )
    }

    fn send_message(&mut self, chat_id: &str, text: &str) -> CallResult<()> {
        Self::read(
            self.client
                .post(format!("{}/sendMessage", self.base))
                .json(&telegram_send_payload(chat_id, text))
                .send(),
            MAX_BRIDGE_POST_RESPONSE_BYTES,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramBotIdentity {
    id: String,
    username: Option<String>,
}

fn bounded_external_field(value: &str, allow_empty: bool) -> Option<String> {
    if (!allow_empty && value.is_empty())
        || value.chars().count() > MAX_BRIDGE_ROUTE_FIELD_LEN
        || value.chars().any(char::is_control)
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn telegram_bot_identity(response: &Value) -> CallResult<TelegramBotIdentity> {
    let result = response
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| local_error("invalid_response", "Telegram returned no bot identity"))?;
    let id = result
        .get("id")
        .and_then(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .map(|id| id.to_string())
                .or_else(|| value.as_u64().filter(|id| *id > 0).map(|id| id.to_string()))
        })
        .and_then(|id| bounded_external_field(&id, false))
        .ok_or_else(|| {
            local_error(
                "invalid_response",
                "Telegram returned an invalid bot identity",
            )
        })?;
    let username = match result.get("username") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|s| bounded_external_field(s, false))
                .ok_or_else(|| {
                    local_error(
                        "invalid_response",
                        "Telegram returned an invalid bot username",
                    )
                })?,
        ),
    };
    Ok(TelegramBotIdentity { id, username })
}

fn validate_configured_bot_username(
    runtime: &TelegramBridgeRuntimeConfig,
    identity: &TelegramBotIdentity,
) -> CallResult<()> {
    if runtime.bot_username.as_deref().is_some_and(|configured| {
        !identity
            .username
            .as_deref()
            .is_some_and(|checked| checked.eq_ignore_ascii_case(configured))
    }) {
        return Err(local_error(
            "identity_mismatch",
            "configured Telegram bot username does not match checked bot identity",
        ));
    }
    Ok(())
}

fn check_with_transport(
    runtime: &TelegramBridgeRuntimeConfig,
    transport: &mut dyn TelegramTransport,
) -> CallResult<BridgeCheck> {
    let identity = telegram_bot_identity(&transport.get_me()?)?;
    validate_configured_bot_username(runtime, &identity)?;
    let external_scope = checked_chat_scope(runtime, transport)?;
    Ok(BridgeCheck {
        platform: BridgePlatform::Telegram,
        external_identity: Some(identity.id),
        external_scope: Some(external_scope),
    })
}

/// Resolve the configured numeric id or @username to Telegram's canonical
/// numeric chat id. Event filtering, provider posts, event keys, and durable
/// cursor binding all use this checked immutable scope, so a later username
/// reassignment cannot split inbound and outbound routing.
fn checked_chat_scope(
    runtime: &TelegramBridgeRuntimeConfig,
    transport: &mut dyn TelegramTransport,
) -> CallResult<String> {
    let chat = transport.get_chat(&runtime.chat_id)?;
    let result = chat
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| local_error("invalid_response", "Telegram returned an invalid chat"))?;
    let returned_id = result
        .get("id")
        .and_then(|id| {
            id.as_i64()
                .map(i128::from)
                .or_else(|| id.as_u64().map(i128::from))
        })
        .ok_or_else(|| local_error("invalid_response", "Telegram returned an invalid chat"))?;
    if let Ok(configured_id) = runtime.chat_id.parse::<i128>() {
        if configured_id != returned_id {
            return Err(local_error(
                "scope_mismatch",
                "configured Telegram chat does not match the checked chat",
            ));
        }
    } else if let Some(configured_username) = runtime.chat_id.strip_prefix('@') {
        let matches = result
            .get("username")
            .and_then(Value::as_str)
            .and_then(|username| bounded_external_field(username, false))
            .is_some_and(|username| username.eq_ignore_ascii_case(configured_username));
        if !matches {
            return Err(local_error(
                "scope_mismatch",
                "configured Telegram chat does not match the checked chat",
            ));
        }
    } else {
        return Err(local_error(
            "invalid_response",
            "configured Telegram chat identifier is invalid",
        ));
    }
    bounded_external_field(&returned_id.to_string(), false)
        .ok_or_else(|| local_error("invalid_response", "Telegram returned an invalid chat"))
}

/// Perform bounded, non-consuming `getMe` and configured-chat checks. Errors
/// contain only local classifications; token/provider strings are absent.
pub fn check(config: &Config) -> Result<BridgeCheck> {
    let runtime = config.telegram_bridge_runtime()?;
    let mut transport = HttpTelegramTransport::new(&runtime.token)?;
    check_with_transport(&runtime, &mut transport)
        .map_err(|error| anyhow::anyhow!("{}: {}", error.class, error.message))
}

/// Structured bot command shared by Telegram and Slack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    Help,
    Inbox,
    Peers,
    Sessions,
    Send { to: String, body: String },
    Ask { to: String, body: String },
    Answer { id: String, body: String },
    Reply { message_id: i64, body: String },
}

impl BotCommand {
    fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Send { .. } | Self::Ask { .. } | Self::Answer { .. } | Self::Reply { .. }
        )
    }
}

/// Parse the common command grammar. Telegram's exact `@bot` addressing check is
/// applied separately by the runtime classifier before this parser is called.
pub fn parse_bot_command(text: &str) -> Option<BotCommand> {
    let first = text.split_whitespace().next().unwrap_or("");
    let rest = first.strip_prefix('/')?;
    let command = rest.split('@').next().unwrap_or(rest);
    Some(match command {
        "inbox" => BotCommand::Inbox,
        "peers" => BotCommand::Peers,
        "sessions" => BotCommand::Sessions,
        "send" => parse_two_arg_command(text)
            .map(|(to, body)| BotCommand::Send { to, body })
            .unwrap_or(BotCommand::Help),
        "ask" => parse_two_arg_command(text)
            .map(|(to, body)| BotCommand::Ask { to, body })
            .unwrap_or(BotCommand::Help),
        "answer" => parse_two_arg_command(text)
            .map(|(id, body)| BotCommand::Answer { id, body })
            .unwrap_or(BotCommand::Help),
        "reply" => match parse_two_arg_command(text) {
            Some((id, body)) => match id.parse::<i64>() {
                Ok(message_id) => BotCommand::Reply { message_id, body },
                Err(_) => BotCommand::Help,
            },
            None => BotCommand::Help,
        },
        _ => BotCommand::Help,
    })
}

fn parse_two_arg_command(text: &str) -> Option<(String, String)> {
    let mut parts = text.splitn(3, char::is_whitespace);
    parts.next()?;
    let first = parts.next()?.trim();
    let rest = parts.next()?.trim();
    (!first.is_empty() && !rest.is_empty()).then(|| (first.to_string(), rest.to_string()))
}

enum TelegramText {
    Ordinary,
    ForeignCommand,
    Command(BotCommand),
}

fn classify_telegram_text(text: &str, bot_username: Option<&str>) -> TelegramText {
    let first = text.split_whitespace().next().unwrap_or("");
    let Some(command) = first.strip_prefix('/') else {
        return TelegramText::Ordinary;
    };
    if let Some((_, suffix)) = command.split_once('@') {
        if !bot_username.is_some_and(|username| username.eq_ignore_ascii_case(suffix)) {
            return TelegramText::ForeignCommand;
        }
    }
    TelegramText::Command(parse_bot_command(text).unwrap_or(BotCommand::Help))
}

pub fn bot_write_commands_enabled(_config: &Config) -> bool {
    matches!(
        std::env::var("WEAVE_BOT_WRITES")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Map a command to the canonical MCP dispatcher. `/inbox` is explicitly a
/// non-marking read here; bridge runtime handling performs its own deferred exact
/// acknowledgement only after the external response is accepted.
pub fn bot_command_rpc(
    command: &BotCommand,
    me: &str,
    allow_writes: bool,
) -> std::result::Result<Option<Value>, String> {
    let (name, arguments) = match command {
        BotCommand::Help => return Ok(None),
        BotCommand::Inbox => (
            "weave_inbox",
            json!({"me": me, "include_read": false, "mark_read": false}),
        ),
        BotCommand::Peers => ("weave_peers", json!({"circle": "*"})),
        BotCommand::Sessions => ("weave_sessions", json!({"circle": "*"})),
        BotCommand::Send { to, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            (
                "weave_send",
                json!({"from": me, "to": to, "body": body, "no_memory": true}),
            )
        }
        BotCommand::Ask { to, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            (
                "weave_ask",
                json!({"from": me, "to": to, "body": body, "no_memory": true}),
            )
        }
        BotCommand::Answer { id, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            (
                "weave_answer",
                json!({"from": me, "correlation_id": id, "body": body, "no_memory": true}),
            )
        }
        BotCommand::Reply { message_id, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            (
                "weave_reply",
                json!({"from": me, "in_reply_to": message_id, "body": body, "no_memory": true}),
            )
        }
    };
    Ok(Some(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })))
}

fn bot_writes_disabled_text() -> String {
    "bot write commands are disabled; set WEAVE_BOT_WRITES=1 to enable /send, /ask, /answer, and /reply".to_string()
}

pub fn bot_help_text() -> String {
    "weave bot commands:\n\
     /inbox — unread messages for this bridge\n\
     /peers — registered peers + presence\n\
     /sessions — known sessions + unread counts\n\
     /send <to> <body> — send a message (requires WEAVE_BOT_WRITES=1)\n\
     /ask <to> <body> — open a tracked ask (requires WEAVE_BOT_WRITES=1)\n\
     /answer <ask_id> <body> — answer an ask (requires WEAVE_BOT_WRITES=1)\n\
     /reply <message_id> <body> — reply to a message (requires WEAVE_BOT_WRITES=1)\n\
     /help — this list"
        .to_string()
}

pub fn format_bot_reply(response: Option<&str>) -> String {
    let Some(raw) = response else {
        return "(no response)".to_string();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if let Some(text) = value
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        return format!("error: {message}");
    }
    raw.to_string()
}

/// Whether a provider event may be durably consumed after the canonical MCP
/// dispatcher returns. The reply text is deliberately separate from this state:
/// control flow never depends on matching a human-facing error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BotDispatchDisposition {
    /// The keyed mesh mutation committed (including an exact replay). Commit the
    /// provider cursor before the best-effort response post.
    DurableMutation,
    /// A read completed or a mutating request was permanently rejected. Post the
    /// canonical response first, then consume the provider event.
    Terminal,
    /// An operational failure left no durable keyed mutation. Do not post or
    /// consume the provider event; a later poll must retry it.
    Retryable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BotDispatchOutcome {
    pub reply: String,
    pub disposition: BotDispatchDisposition,
}

impl BotDispatchOutcome {
    #[cfg(test)]
    pub(crate) fn durable(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            disposition: BotDispatchDisposition::DurableMutation,
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            disposition: BotDispatchDisposition::Terminal,
        }
    }

    #[cfg(test)]
    pub(crate) fn retryable() -> Self {
        Self {
            reply: String::new(),
            disposition: BotDispatchDisposition::Retryable,
        }
    }
}

fn recipient_failure_disposition(store: &dyn Store, to: &str) -> Option<BotDispatchDisposition> {
    if store::check_ident("recipient", to).is_err() {
        return Some(BotDispatchDisposition::Terminal);
    }
    if !to.starts_with("sess_") {
        return None;
    }
    if !weave_core::model::session_id_valid(to) {
        return Some(BotDispatchDisposition::Terminal);
    }
    match store.list_peers() {
        Err(_) => Some(BotDispatchDisposition::Retryable),
        Ok(peers) => {
            let matches = peers
                .iter()
                .filter(|peer| weave_core::model::peer_session_id(peer) == to)
                .count();
            (matches != 1).then_some(BotDispatchDisposition::Terminal)
        }
    }
}

fn failed_mutation_disposition(
    store: &dyn Store,
    identity: &str,
    command: &BotCommand,
    idempotency_key: Option<&str>,
) -> BotDispatchDisposition {
    if let Some(key) = idempotency_key {
        match store.message_by_idempotency_key(key) {
            // Canonical keyed tool success and exact replay both return
            // isError=false. Seeing the key only on an error means the event key
            // belongs to different semantics, so this is a permanent collision
            // whose rejection must be posted before the cursor advances.
            Ok(Some(_)) => return BotDispatchDisposition::Terminal,
            Err(_) => return BotDispatchDisposition::Retryable,
            Ok(None) => {}
        }
    }
    if store::check_ident("sender", identity).is_err() {
        return BotDispatchDisposition::Terminal;
    }
    match command {
        BotCommand::Send { to, body } => {
            if store::check_body(body).is_err() {
                BotDispatchDisposition::Terminal
            } else {
                recipient_failure_disposition(store, to)
                    .unwrap_or(BotDispatchDisposition::Retryable)
            }
        }
        BotCommand::Ask { to, body } => {
            if store::check_body(body).is_err() || weave_core::model::is_broadcast(to) {
                BotDispatchDisposition::Terminal
            } else {
                recipient_failure_disposition(store, to)
                    .unwrap_or(BotDispatchDisposition::Retryable)
            }
        }
        BotCommand::Answer { id, body } => {
            if store::check_body(body).is_err() || !weave_core::model::ask_id_valid(id) {
                return BotDispatchDisposition::Terminal;
            }
            match store.get_ask(id) {
                Err(_) => BotDispatchDisposition::Retryable,
                Ok(None) => BotDispatchDisposition::Terminal,
                Ok(Some(ask)) if ask.askee != identity || ask.state != AskState::Open => {
                    BotDispatchDisposition::Terminal
                }
                Ok(Some(_)) => BotDispatchDisposition::Retryable,
            }
        }
        BotCommand::Reply { message_id, body } => {
            if *message_id <= 0 || store::check_body(body).is_err() {
                return BotDispatchDisposition::Terminal;
            }
            match store.message_exists(*message_id) {
                Ok(false) => BotDispatchDisposition::Terminal,
                Ok(true) | Err(_) => BotDispatchDisposition::Retryable,
            }
        }
        BotCommand::Help | BotCommand::Inbox | BotCommand::Peers | BotCommand::Sessions => {
            BotDispatchDisposition::Terminal
        }
    }
}

fn mcp_response_failed(response: &str) -> Option<bool> {
    let value = serde_json::from_str::<Value>(response).ok()?;
    if value.get("error").is_some() {
        return Some(true);
    }
    value.pointer("/result/isError").and_then(Value::as_bool)
}

#[allow(dead_code)]
pub fn dispatch_bot_command(
    store: &dyn Store,
    config: &Config,
    identity: &str,
    command: &BotCommand,
    injector: &dyn weave_inject::Injector,
) -> String {
    dispatch_bot_command_with_key(store, config, identity, command, injector, None).reply
}

pub(crate) fn dispatch_bot_command_with_key(
    store: &dyn Store,
    config: &Config,
    identity: &str,
    command: &BotCommand,
    injector: &dyn weave_inject::Injector,
    idempotency_key: Option<&str>,
) -> BotDispatchOutcome {
    let allow_writes = bot_write_commands_enabled(config);
    match bot_command_rpc(command, identity, allow_writes) {
        Ok(None) => BotDispatchOutcome {
            reply: bot_help_text(),
            disposition: BotDispatchDisposition::Terminal,
        },
        Err(error) => BotDispatchOutcome {
            reply: error,
            disposition: BotDispatchDisposition::Terminal,
        },
        Ok(Some(mut rpc)) => {
            // Every mutating command carries the route-bound provider event key
            // into its canonical Store transaction. An exact replay therefore
            // returns the original result without a second nudge or hook.
            if command.mutates() {
                if let (Some(key), Some(arguments)) = (
                    idempotency_key,
                    rpc.pointer_mut("/params/arguments")
                        .and_then(Value::as_object_mut),
                ) {
                    arguments.insert("idempotencyKey".to_string(), json!(key));
                }
            }
            let response = weave_mcp::mcp::dispatch_request(
                store,
                &Some(identity.to_string()),
                None,
                &[],
                &weave_mcp::PullConsent::empty(),
                &rpc,
                injector,
                allow_writes,
            );
            let reply = format_bot_reply(response.as_deref());
            let disposition = if !command.mutates() {
                BotDispatchDisposition::Terminal
            } else {
                match response.as_deref().and_then(mcp_response_failed) {
                    Some(false) => BotDispatchDisposition::DurableMutation,
                    Some(true) => {
                        failed_mutation_disposition(store, identity, command, idempotency_key)
                    }
                    None => BotDispatchDisposition::Retryable,
                }
            };
            BotDispatchOutcome { reply, disposition }
        }
    }
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    const MARKER: &str = " … [truncated]";
    let marker_chars = MARKER.chars().count();
    if marker_chars >= max_chars {
        return MARKER.chars().take(max_chars).collect();
    }
    let mut value: String = text.chars().take(max_chars - marker_chars).collect();
    value.push_str(MARKER);
    value
}

fn bounded_body(platform: &str, raw_sender: &str, text: &str) -> String {
    let sender = sanitize_inbound_ident(raw_sender, "user");
    let prefix = format!("[{platform}:{sender}] ");
    let mut body = String::with_capacity(prefix.len() + text.len().min(MAX_BODY));
    body.push_str(&prefix);
    let available = MAX_BODY.saturating_sub(body.len());
    if text.len() <= available {
        body.push_str(text);
    } else {
        let mut boundary = available;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        body.push_str(&text[..boundary]);
    }
    body
}

fn stable_event_key(
    platform: &str,
    external_identity: &str,
    scope: &str,
    event_id: &str,
) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for component in [external_identity, scope] {
        for byte in component.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Component boundary prevents ambiguous concatenations such as
        // (`ab`, `c`) and (`a`, `bc`) from sharing a route digest.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("bridge:{platform}:{hash:016x}:{event_id}")
}

fn telegram_sender(message: &Value) -> String {
    let from = message.get("from");
    from.and_then(|value| value.get("username"))
        .and_then(Value::as_str)
        .or_else(|| {
            from.and_then(|value| value.get("first_name"))
                .and_then(Value::as_str)
        })
        .map(|value| sanitize_inbound_ident(value, "telegram-user"))
        .unwrap_or_else(|| "telegram-user".to_string())
}

fn telegram_chat_id(message: &Value) -> Option<String> {
    let value = message.pointer("/chat/id")?;
    let rendered = if let Some(id) = value.as_i64() {
        id.to_string()
    } else if let Some(id) = value.as_u64() {
        id.to_string()
    } else {
        value.as_str()?.to_string()
    };
    bounded_external_field(&rendered, false)
}

fn telegram_update_id(update: &Value) -> Option<i64> {
    update.get("update_id")?.as_i64().filter(|id| *id >= 0)
}

fn encode_cursor(identity: &str, chat_id: &str, position: i64) -> CallResult<String> {
    BridgeCursorEnvelope {
        external_identity: identity.to_string(),
        external_scope: chat_id.to_string(),
        position: position.to_string(),
        continuation: None,
    }
    .encode()
    .map_err(|_| local_error("cursor_invalid", "Telegram cursor could not be encoded"))
}

fn decode_cursor(cursor: &str, identity: &str, chat_id: &str) -> CallResult<Option<i64>> {
    let Some(envelope) = decode_cursor_strict(cursor)? else {
        return Ok(None);
    };
    // A valid cursor for another checked route is intentionally reset. A cursor
    // claiming this route but carrying a foreign continuation or malformed
    // position is corruption and must stop before getUpdates can consume data.
    if !envelope.route_matches(identity, chat_id) {
        return Ok(None);
    }
    if envelope.continuation.is_some() {
        return Err(local_error(
            "cursor",
            "Telegram cursor state is not supported",
        ));
    }
    let position = envelope
        .position
        .parse::<i64>()
        .ok()
        .filter(|id| *id >= 0)
        .ok_or_else(|| local_error("cursor", "Telegram cursor position is invalid"))?;
    Ok(Some(position))
}

fn persist_cursor(
    store: &dyn Store,
    owner_id: &str,
    external_identity: &str,
    chat_id: &str,
    position: i64,
) -> CallResult<String> {
    let encoded = encode_cursor(external_identity, chat_id, position)?;
    let updated = store
        .update_bridge_runtime(
            BridgePlatform::Telegram,
            owner_id,
            &BridgeRuntimeUpdate {
                cursor: Some(encoded.clone()),
                ..BridgeRuntimeUpdate::default()
            },
        )
        .map_err(|_| local_error("local_store", "Telegram cursor update failed"))?;
    if !updated {
        return Err(local_error(
            "ownership_lost",
            "Telegram bridge runtime ownership was lost",
        ));
    }
    Ok(encoded)
}

fn heartbeat_owned(store: &dyn Store, owner_id: &str) -> CallResult<()> {
    let owned = store
        .update_bridge_runtime(
            BridgePlatform::Telegram,
            owner_id,
            &BridgeRuntimeUpdate::default(),
        )
        .map_err(|_| local_error("local_store", "Telegram bridge heartbeat failed"))?;
    if !owned {
        return Err(local_error(
            "ownership_lost",
            "Telegram bridge runtime ownership was lost",
        ));
    }
    Ok(())
}

#[derive(Debug, Default)]
struct PollReport {
    delivered: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    bootstrapped: bool,
}

fn bootstrap_once(
    store: &dyn Store,
    external_identity: &str,
    external_scope: &str,
    owner_id: &str,
    cursor: &mut String,
    transport: &mut dyn TelegramTransport,
) -> CallResult<PollReport> {
    let response = transport.get_updates(-1, 0)?;
    let updates = response
        .get("result")
        .and_then(Value::as_array)
        .ok_or_else(|| local_error("invalid_response", "Telegram returned invalid updates"))?;
    if updates.len() > MAX_TELEGRAM_UPDATES {
        return Err(local_error(
            "invalid_response",
            "Telegram returned too many updates",
        ));
    }
    let mut latest = 0_i64;
    for update in updates {
        let id = telegram_update_id(update).ok_or_else(|| {
            local_error("invalid_response", "Telegram returned an invalid update id")
        })?;
        latest = latest.max(id);
    }
    *cursor = persist_cursor(store, owner_id, external_identity, external_scope, latest)?;
    Ok(PollReport {
        bootstrapped: true,
        ..PollReport::default()
    })
}

fn inbox_reply(store: &dyn Store, identity: &str) -> CallResult<(Vec<Message>, String)> {
    let (available, remaining) = store
        .inbox(identity, false, false, MAX_INBOX_SNAPSHOT)
        .map_err(|_| local_error("local_store", "Telegram inbox read failed"))?;
    if available.is_empty() {
        return Ok((
            Vec::new(),
            weave_mcp::mcp::format_inbox_rows(identity, &[], 0, false, false),
        ));
    }
    let mut selected = Vec::new();
    for message in available.iter().cloned() {
        let mut candidate = selected.clone();
        candidate.push(message);
        let rendered =
            weave_mcp::mcp::format_inbox_rows(identity, &candidate, remaining, false, false);
        if !selected.is_empty() && rendered.chars().count() > MAX_BRIDGE_TEXT_CHARS {
            break;
        }
        selected = candidate;
    }
    let rendered = weave_mcp::mcp::format_inbox_rows(identity, &selected, remaining, false, false);
    Ok((selected, bounded_text(&rendered, MAX_BRIDGE_TEXT_CHARS)))
}

fn complete_inbox_snapshot(
    store: &dyn Store,
    owner_id: &str,
    external_identity: &str,
    external_scope: &str,
    update_id: i64,
    identity: &str,
    rows: &[Message],
) -> CallResult<String> {
    let encoded = encode_cursor(external_identity, external_scope, update_id)?;
    let message_ids = rows.iter().map(|message| message.id).collect::<Vec<_>>();
    let completed = store
        .complete_bridge_inbox_snapshot(
            BridgePlatform::Telegram,
            owner_id,
            identity,
            &message_ids,
            &BridgeRuntimeUpdate {
                cursor: Some(encoded.clone()),
                ..BridgeRuntimeUpdate::default()
            },
        )
        .map_err(|_| {
            local_error(
                "local_ack",
                "Telegram inbox cursor and acknowledgement transaction failed",
            )
        })?;
    if !completed {
        return Err(local_error(
            "ownership_lost",
            "Telegram bridge runtime ownership was lost",
        ));
    }
    Ok(encoded)
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
fn poll_once(
    store: &dyn Store,
    config: &Config,
    runtime: &TelegramBridgeRuntimeConfig,
    bot: &TelegramBotIdentity,
    owner_id: &str,
    cursor: &mut String,
    transport: &mut dyn TelegramTransport,
    injector: &dyn weave_inject::Injector,
) -> CallResult<PollReport> {
    let mut dispatch = |command: &BotCommand, idempotency_key: Option<&str>| {
        dispatch_bot_command_with_key(
            store,
            config,
            &runtime.identity,
            command,
            injector,
            idempotency_key,
        )
    };
    poll_once_scoped(
        store,
        runtime,
        bot,
        &runtime.chat_id,
        owner_id,
        cursor,
        transport,
        &mut dispatch,
    )
}

#[allow(clippy::too_many_arguments)]
fn poll_once_scoped<D>(
    store: &dyn Store,
    runtime: &TelegramBridgeRuntimeConfig,
    bot: &TelegramBotIdentity,
    external_scope: &str,
    owner_id: &str,
    cursor: &mut String,
    transport: &mut dyn TelegramTransport,
    dispatch: &mut D,
) -> CallResult<PollReport>
where
    D: FnMut(&BotCommand, Option<&str>) -> BotDispatchOutcome,
{
    let Some(mut last_update_id) = decode_cursor(cursor, &bot.id, external_scope)? else {
        return bootstrap_once(store, &bot.id, external_scope, owner_id, cursor, transport);
    };
    let response = transport.get_updates(last_update_id.saturating_add(1), POLL_TIMEOUT_SECS)?;
    let updates = response
        .get("result")
        .and_then(Value::as_array)
        .ok_or_else(|| local_error("invalid_response", "Telegram returned invalid updates"))?;
    if updates.len() > MAX_TELEGRAM_UPDATES {
        return Err(local_error(
            "invalid_response",
            "Telegram returned too many updates",
        ));
    }
    let mut ordered = Vec::with_capacity(updates.len());
    for update in updates {
        let id = telegram_update_id(update).ok_or_else(|| {
            local_error("invalid_response", "Telegram returned an invalid update id")
        })?;
        ordered.push((id, update));
    }
    ordered.sort_by_key(|(id, _)| *id);

    let mut report = PollReport::default();
    let mut command_replied = false;
    for (update_id, update) in ordered {
        if update_id <= last_update_id {
            continue;
        }
        let message = update.get("message");
        let allowed = message
            .and_then(telegram_chat_id)
            .is_some_and(|chat_id| chat_id == external_scope);
        if !allowed {
            *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            last_update_id = update_id;
            continue;
        }
        let Some(message) = message else {
            *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            last_update_id = update_id;
            continue;
        };
        if message
            .pointer("/from/is_bot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            last_update_id = update_id;
            continue;
        }
        let Some(text) = message.get("text").and_then(Value::as_str) else {
            *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            last_update_id = update_id;
            continue;
        };
        if text.is_empty() {
            *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            last_update_id = update_id;
            continue;
        }
        let event_key =
            stable_event_key("telegram", &bot.id, external_scope, &update_id.to_string());
        match classify_telegram_text(text, bot.username.as_deref()) {
            TelegramText::ForeignCommand => {
                *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
            }
            TelegramText::Ordinary => {
                let body = bounded_body("telegram", &telegram_sender(message), text);
                // Fence immediately before the local write. A worker whose lease
                // was reclaimed while getUpdates was in flight must not commit an
                // event under stale route configuration and only discover the
                // ownership loss while persisting the cursor.
                heartbeat_owned(store, owner_id)?;
                store
                    .send(
                        &runtime.identity,
                        &runtime.recipient,
                        None,
                        &body,
                        Some(&event_key),
                        None,
                    )
                    .map_err(|_| local_error("local_store", "Telegram inbound relay failed"))?;
                *cursor = persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
                report.delivered = true;
            }
            TelegramText::Command(BotCommand::Inbox) => {
                let (rows, reply) = inbox_reply(store, &runtime.identity)?;
                heartbeat_owned(store, owner_id)?;
                transport.send_message(external_scope, &reply)?;
                // Telegram does not accept a client idempotency key for sendMessage,
                // so a crash after provider acceptance but before this local commit
                // can re-post the snapshot. Once execution reaches this seam, the
                // exact receipts and event cursor commit atomically under the owner
                // fence: no later crash can advance one without the other.
                *cursor = complete_inbox_snapshot(
                    store,
                    owner_id,
                    &bot.id,
                    external_scope,
                    update_id,
                    &runtime.identity,
                    &rows,
                )?;
                report.delivered = true;
                command_replied = true;
            }
            TelegramText::Command(command) => {
                // Fence immediately before the shared dispatcher: a stale owner
                // must never mutate the mesh and only then discover that another
                // process reclaimed this bridge.
                heartbeat_owned(store, owner_id)?;
                let outcome = dispatch(&command, Some(&event_key));
                let reply = bounded_text(&outcome.reply, MAX_BRIDGE_TEXT_CHARS);
                match outcome.disposition {
                    BotDispatchDisposition::DurableMutation => {
                        // The Store mutation is atomically keyed. Persist local
                        // handling before the best-effort response so neither the
                        // mutation nor the provider response repeats after restart.
                        *cursor =
                            persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
                        heartbeat_owned(store, owner_id)?;
                        transport.send_message(external_scope, &reply)?;
                    }
                    BotDispatchDisposition::Terminal => {
                        // Read results and permanent rejections have no provider
                        // idempotency key. Post first so a local cursor failure cannot
                        // lose the only response; the acceptance-to-cursor window is
                        // intentionally at-least-once and may duplicate after a crash.
                        heartbeat_owned(store, owner_id)?;
                        transport.send_message(external_scope, &reply)?;
                        *cursor =
                            persist_cursor(store, owner_id, &bot.id, external_scope, update_id)?;
                    }
                    BotDispatchDisposition::Retryable => {
                        return Err(local_error(
                            "dispatch_retryable",
                            "Telegram command dispatch failed before durable acceptance",
                        ));
                    }
                }
                report.delivered = true;
                command_replied = true;
            }
        }
        last_update_id = update_id;
    }

    // A command response is already this iteration's external delivery. Defer the
    // automatic inbox relay to the next poll so `/inbox` acknowledges exactly the
    // rows represented by its accepted snapshot and does not implicitly drain an
    // adjacent row in the same command transaction.
    if command_replied {
        return Ok(report);
    }

    let mut post_error = None;
    let outbound = relay_outbound_once(
        store,
        BridgePlatform::Telegram,
        &runtime.identity,
        MAX_BRIDGE_BATCH,
        |text| {
            let result = (|| {
                heartbeat_owned(store, owner_id)?;
                transport.send_message(external_scope, text)
            })();
            if let Err(error) = &result {
                post_error = Some(error.clone());
            }
            result
        },
    )
    .map_err(|_| local_error("local_store", "Telegram outbound relay failed"))?;
    if let Some(error) = post_error {
        return Err(error);
    }
    if let (Some(class), Some(message)) = (outbound.error_class, outbound.error) {
        return Err(BridgeCallError::new(class, message));
    }
    report.delivered |= outbound.delivered > 0;
    Ok(report)
}

fn owner_id() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("telegram-{}-{nonce:x}", std::process::id())
}

fn update_health(
    store: &dyn Store,
    owner_id: &str,
    status: BridgeRuntimeStatus,
    delivered: bool,
    error: BridgeRuntimeErrorUpdate,
) -> Result<bool> {
    let timestamp = now();
    store.update_bridge_runtime(
        BridgePlatform::Telegram,
        owner_id,
        &BridgeRuntimeUpdate {
            status: Some(status),
            last_poll_ts: Some(timestamp),
            last_success_ts: (status == BridgeRuntimeStatus::Running).then_some(timestamp),
            last_delivery_ts: delivered.then_some(timestamp),
            error,
            ..BridgeRuntimeUpdate::default()
        },
    )
}

fn run_claimed(
    store: &dyn Store,
    config: &Config,
    runtime: &TelegramBridgeRuntimeConfig,
    owner_id: &str,
    cursor: &mut String,
    transport: &mut dyn TelegramTransport,
) -> Result<()> {
    let injector = crate::RealInjector {
        preferred_mux: crate::parse_mux_preference(config),
    };
    let (bot, external_scope) = loop {
        let checked = transport
            .get_me()
            .and_then(|value| telegram_bot_identity(&value))
            .and_then(|identity| {
                validate_configured_bot_username(runtime, &identity)?;
                let external_scope = checked_chat_scope(runtime, transport)?;
                Ok((identity, external_scope))
            });
        match checked {
            Ok(route) => break route,
            Err(error) => {
                if !update_health(
                    store,
                    owner_id,
                    BridgeRuntimeStatus::Degraded,
                    false,
                    BridgeRuntimeErrorUpdate::Set {
                        class: error.class.clone(),
                        message: error.message.clone(),
                    },
                )? {
                    anyhow::bail!("Telegram bridge runtime ownership was lost");
                }
                eprintln!("[weave-telegram] {}: {}", error.class, error.message);
                std::thread::sleep(Duration::from_secs(RETRY_DELAY_SECS));
            }
        }
    };
    eprintln!(
        "[weave-telegram] bridge started as identity '{}'",
        runtime.identity
    );
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
        match poll_once_scoped(
            store,
            runtime,
            &bot,
            &external_scope,
            owner_id,
            cursor,
            transport,
            &mut dispatch,
        ) {
            Ok(report) => {
                if !update_health(
                    store,
                    owner_id,
                    BridgeRuntimeStatus::Running,
                    report.delivered,
                    BridgeRuntimeErrorUpdate::Clear,
                )? {
                    anyhow::bail!("Telegram bridge runtime ownership was lost");
                }
            }
            Err(error) if error.class == "ownership_lost" => {
                anyhow::bail!("Telegram bridge runtime ownership was lost");
            }
            Err(error) => {
                if !update_health(
                    store,
                    owner_id,
                    BridgeRuntimeStatus::Degraded,
                    false,
                    BridgeRuntimeErrorUpdate::Set {
                        class: error.class.clone(),
                        message: error.message.clone(),
                    },
                )? {
                    anyhow::bail!("Telegram bridge runtime ownership was lost");
                }
                eprintln!("[weave-telegram] {}: {}", error.class, error.message);
            }
        }
        std::thread::sleep(Duration::from_secs(RETRY_DELAY_SECS));
    }
}

fn run_with_transport_factory<T, F>(store: &dyn Store, config: &Config, factory: F) -> Result<()>
where
    T: TelegramTransport,
    F: FnOnce(&str) -> Result<T>,
{
    let runtime = config.telegram_bridge_runtime()?;
    // Construct every fallible transport resource before acquiring the durable
    // runtime lease. Once claim succeeds, all normal return paths below pass
    // through `release_bridge_runtime`.
    let mut transport = factory(&runtime.token)?;
    let owner_id = owner_id();
    let owner_host = weave_core::config::this_host();
    let state = store
        .claim_bridge_runtime(
            BridgePlatform::Telegram,
            &runtime.identity,
            &runtime.recipient,
            &owner_id,
            Some(i64::from(std::process::id())),
            &owner_host,
            now().saturating_sub(BRIDGE_ACTIVE_TTL_SECS),
        )?
        .context("Telegram bridge is already active")?;
    let mut cursor = state.cursor;
    let result = run_claimed(
        store,
        config,
        &runtime,
        &owner_id,
        &mut cursor,
        &mut transport,
    );
    let release = store.release_bridge_runtime(BridgePlatform::Telegram, &owner_id);
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(true)) => Ok(()),
        (Ok(()), Ok(false)) => anyhow::bail!("Telegram bridge runtime ownership was lost"),
        (Ok(()), Err(error)) => Err(error.context("releasing Telegram bridge runtime")),
    }
}

/// Run the Telegram bridge on the calling thread.
pub fn run(store: &dyn Store, config: &Config) -> Result<()> {
    run_with_transport_factory(store, config, HttpTelegramTransport::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(feature = "sqlite")]
    use weave_core::store::SqliteStore;
    #[cfg(feature = "libsql")]
    use weave_core::store_libsql::LibsqlStore;

    #[cfg(feature = "sqlite")]
    type TestStore = SqliteStore;
    #[cfg(feature = "libsql")]
    type TestStore = LibsqlStore;

    static NEXT_DB: AtomicU64 = AtomicU64::new(1);

    fn store() -> (TestStore, PathBuf) {
        let n = NEXT_DB.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "weave-telegram-runtime-{}-{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        #[cfg(feature = "sqlite")]
        let store = SqliteStore::open(&path).unwrap();
        #[cfg(feature = "libsql")]
        let store = LibsqlStore::open(&Config {
            db: Some(path.to_string_lossy().into_owned()),
            ..Config::default()
        })
        .unwrap();
        (store, path)
    }

    fn runtime() -> TelegramBridgeRuntimeConfig {
        TelegramBridgeRuntimeConfig {
            token: "test-token".into(),
            chat_id: "-1001".into(),
            identity: "telegram".into(),
            recipient: "worker".into(),
            bot_username: Some("weave_bot".into()),
        }
    }

    fn bot(id: &str) -> TelegramBotIdentity {
        TelegramBotIdentity {
            id: id.into(),
            username: Some("weave_bot".into()),
        }
    }

    fn claim(store: &TestStore, runtime: &TelegramBridgeRuntimeConfig) -> String {
        let owner = format!("test-owner-{}", NEXT_DB.fetch_add(1, Ordering::Relaxed));
        store
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                &runtime.identity,
                &runtime.recipient,
                &owner,
                Some(1),
                "test-host",
                0,
            )
            .unwrap()
            .unwrap();
        owner
    }

    struct FakeTransport {
        me: CallResult<Value>,
        chat: CallResult<Value>,
        updates: VecDeque<CallResult<Value>>,
        posts: VecDeque<CallResult<()>>,
        update_requests: Vec<(i64, u64)>,
        posted: Vec<(String, String)>,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                me: Ok(json!({"ok": true, "result": {"id": 7, "username": "weave_bot"}})),
                chat: Ok(json!({"ok": true, "result": {"id": -1001}})),
                updates: VecDeque::new(),
                posts: VecDeque::new(),
                update_requests: Vec::new(),
                posted: Vec::new(),
            }
        }
    }

    impl TelegramTransport for FakeTransport {
        fn get_me(&mut self) -> CallResult<Value> {
            self.me.clone()
        }

        fn get_chat(&mut self, _chat_id: &str) -> CallResult<Value> {
            self.chat.clone()
        }

        fn get_updates(&mut self, offset: i64, timeout_secs: u64) -> CallResult<Value> {
            self.update_requests.push((offset, timeout_secs));
            self.updates
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"ok": true, "result": []})))
        }

        fn send_message(&mut self, chat_id: &str, text: &str) -> CallResult<()> {
            self.posted.push((chat_id.to_string(), text.to_string()));
            self.posts.pop_front().unwrap_or(Ok(()))
        }
    }

    /// Test transport that lets Telegram accept a response and then makes one
    /// selected local row ineligible before the cursor/receipt transaction. This
    /// deterministically exercises the otherwise tiny post-acceptance ack-failure
    /// window against either TestStore backend.
    struct ExpireOnAcceptedPost<'a> {
        inner: FakeTransport,
        store: &'a TestStore,
        message_id: i64,
    }

    impl TelegramTransport for ExpireOnAcceptedPost<'_> {
        fn get_me(&mut self) -> CallResult<Value> {
            self.inner.get_me()
        }

        fn get_chat(&mut self, chat_id: &str) -> CallResult<Value> {
            self.inner.get_chat(chat_id)
        }

        fn get_updates(&mut self, offset: i64, timeout_secs: u64) -> CallResult<Value> {
            self.inner.get_updates(offset, timeout_secs)
        }

        fn send_message(&mut self, chat_id: &str, text: &str) -> CallResult<()> {
            self.inner.send_message(chat_id, text)?;
            self.store
                .set_message_expiry(self.message_id, now() - 1)
                .expect("expire selected row after provider acceptance");
            Ok(())
        }
    }

    fn no_injector() -> crate::RealInjector {
        crate::RealInjector {
            preferred_mux: None,
        }
    }

    fn set_cursor(
        store: &TestStore,
        owner: &str,
        identity: &str,
        chat: &str,
        position: i64,
    ) -> String {
        persist_cursor(store, owner, identity, chat, position).unwrap()
    }

    #[test]
    fn payloads_and_parser_are_stable() {
        assert_eq!(
            telegram_send_payload("1", "hi"),
            json!({"chat_id":"1","text":"hi"})
        );
        assert_eq!(
            telegram_get_updates_payload(8, 25),
            json!({"offset":8,"timeout":25})
        );
        let update = json!({"message":{"text":"hi","from":{"username":"alice"}}});
        assert_eq!(
            parse_telegram_update(&update),
            Some(("alice".into(), "hi".into()))
        );
        assert_ne!(
            stable_event_key("telegram", "bot-1", "-1001", "42"),
            stable_event_key("telegram", "bot-2", "-1001", "42")
        );
    }

    #[test]
    fn exact_command_suffix_matching() {
        assert!(matches!(
            classify_telegram_text("/inbox@weave_bot", Some("weave_bot")),
            TelegramText::Command(BotCommand::Inbox)
        ));
        assert!(matches!(
            classify_telegram_text("/inbox@WEAVE_BOT", Some("weave_bot")),
            TelegramText::Command(BotCommand::Inbox)
        ));
        assert!(matches!(
            classify_telegram_text("/inbox@other_bot", Some("weave_bot")),
            TelegramText::ForeignCommand
        ));
        assert!(matches!(
            classify_telegram_text("/inbox@weave_bot", None),
            TelegramText::ForeignCommand
        ));
    }

    #[test]
    fn command_rpc_uses_canonical_keys_and_nonmarking_inbox() {
        let inbox = bot_command_rpc(&BotCommand::Inbox, "bridge", false)
            .unwrap()
            .unwrap();
        assert_eq!(inbox["params"]["arguments"]["mark_read"], false);
        let answer = bot_command_rpc(
            &BotCommand::Answer {
                id: "ask_1".into(),
                body: "yes".into(),
            },
            "bridge",
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(answer["params"]["arguments"]["correlation_id"], "ask_1");
        assert!(answer["params"]["arguments"].get("id").is_none());
        let reply = bot_command_rpc(
            &BotCommand::Reply {
                message_id: 42,
                body: "ack".into(),
            },
            "bridge",
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply["params"]["arguments"]["in_reply_to"], 42);
        assert!(reply["params"]["arguments"].get("message_id").is_none());
        assert_eq!(reply["params"]["arguments"]["no_memory"], true);
    }

    #[test]
    fn dispatcher_replay_returns_original_ask_without_a_second_write() {
        let _env = weave_core::testenv::lock_env();
        let _writes = weave_core::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let command = BotCommand::Ask {
            to: "worker".to_string(),
            body: "question".to_string(),
        };
        let first = dispatch_bot_command_with_key(
            &store,
            &Config::default(),
            "telegram",
            &command,
            &no_injector(),
            Some("telegram-event-ask"),
        );
        let replay = dispatch_bot_command_with_key(
            &store,
            &Config::default(),
            "telegram",
            &command,
            &no_injector(),
            Some("telegram-event-ask"),
        );
        assert!(first.reply.contains("Opened ask"));
        assert_eq!(first.disposition, BotDispatchDisposition::DurableMutation);
        assert!(replay.reply.contains("idempotent replay"));
        assert_eq!(replay.disposition, BotDispatchDisposition::DurableMutation);
        assert_eq!(store.all_messages(10).unwrap().len(), 1);
        assert_eq!(
            store
                .list_asks("telegram", weave_core::model::AskRole::Any, 10)
                .unwrap()
                .len(),
            1
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retryable_command_retries_to_exactly_one_durable_success() {
        let _env = weave_core::testenv::lock_env();
        let _writes = weave_core::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let initial = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 10);
        let update = json!({"ok":true,"result":[{
            "update_id":11,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/send worker once"}
        }]});
        let mut cursor = initial.clone();
        let calls = std::cell::Cell::new(0);
        let mut dispatch = |command: &BotCommand, key: Option<&str>| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                BotDispatchOutcome::retryable()
            } else {
                dispatch_bot_command_with_key(
                    &store,
                    &Config::default(),
                    &rt.identity,
                    command,
                    &no_injector(),
                    key,
                )
            }
        };

        let mut first = FakeTransport::new();
        first.updates.push_back(Ok(update.clone()));
        let error = poll_once_scoped(
            &store,
            &rt,
            &bot,
            &rt.chat_id,
            &owner,
            &mut cursor,
            &mut first,
            &mut dispatch,
        )
        .unwrap_err();
        assert_eq!(error.class, "dispatch_retryable");
        assert_eq!(cursor, initial);
        assert!(first.posted.is_empty());
        assert!(store
            .inbox("worker", false, false, 10)
            .unwrap()
            .0
            .is_empty());

        let mut second = FakeTransport::new();
        second.updates.push_back(Ok(update.clone()));
        poll_once_scoped(
            &store,
            &rt,
            &bot,
            &rt.chat_id,
            &owner,
            &mut cursor,
            &mut second,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(second.posted.len(), 1);
        assert_eq!(store.inbox("worker", false, false, 10).unwrap().0.len(), 1);
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(11)
        );

        let mut already_consumed = FakeTransport::new();
        already_consumed.updates.push_back(Ok(update));
        poll_once_scoped(
            &store,
            &rt,
            &bot,
            &rt.chat_id,
            &owner,
            &mut cursor,
            &mut already_consumed,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(already_consumed.posted.is_empty());
        assert_eq!(store.inbox("worker", false, false, 10).unwrap().0.len(), 1);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn permanent_unknown_reply_and_answer_post_then_advance() {
        let _env = weave_core::testenv::lock_env();
        let _writes = weave_core::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 20);
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[
            {"update_id":21,"message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/reply 999 missing"}},
            {"update_id":22,"message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/answer ask_999_1 missing"}}
        ]})));
        let mut dispatch = |command: &BotCommand, key: Option<&str>| {
            dispatch_bot_command_with_key(
                &store,
                &Config::default(),
                &rt.identity,
                command,
                &no_injector(),
                key,
            )
        };
        poll_once_scoped(
            &store,
            &rt,
            &bot,
            &rt.chat_id,
            &owner,
            &mut cursor,
            &mut transport,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(transport.posted.len(), 2);
        assert!(transport
            .posted
            .iter()
            .all(|(_, reply)| reply.contains("Error:")));
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(22)
        );
        assert!(store.all_messages(10).unwrap().is_empty());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn changed_command_body_key_collision_posts_rejection_before_cursor_commit() {
        let _env = weave_core::testenv::lock_env();
        let _writes = weave_core::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let initial = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 40);
        let event_key = stable_event_key("telegram", &bot.id, &rt.chat_id, "41");
        store
            .send(
                &rt.identity,
                "worker",
                None,
                "first accepted body",
                Some(&event_key),
                None,
            )
            .unwrap();
        let update = json!({"ok":true,"result":[{
            "update_id":41,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/send worker changed body"}
        }]});
        let mut cursor = initial.clone();
        let mut failed = FakeTransport::new();
        failed.updates.push_back(Ok(update.clone()));
        failed.posts.push_back(Err(BridgeCallError::new(
            "transport",
            "rejection post failed",
        )));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut failed,
            &no_injector(),
        )
        .is_err());
        assert_eq!(cursor, initial);
        assert_eq!(store.all_messages(10).unwrap().len(), 1);

        let mut accepted = FakeTransport::new();
        accepted.updates.push_back(Ok(update));
        poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut accepted,
            &no_injector(),
        )
        .unwrap();
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(41)
        );
        assert_eq!(accepted.posted.len(), 1);
        assert!(accepted.posted[0].1.contains("Error:"));
        assert_eq!(store.all_messages(10).unwrap().len(), 1);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn future_cursor_fails_closed_before_provider_progress() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let future = json!({
            "external_identity":"7",
            "external_scope":"-1001",
            "position":"10",
            "version":2
        })
        .to_string();
        assert!(store
            .update_bridge_runtime(
                BridgePlatform::Telegram,
                &owner,
                &BridgeRuntimeUpdate {
                    cursor: Some(future.clone()),
                    ..BridgeRuntimeUpdate::default()
                },
            )
            .unwrap());
        let mut cursor = future.clone();
        let mut transport = FakeTransport::new();
        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "cursor");
        assert_eq!(cursor, future);
        assert!(transport.update_requests.is_empty());
        assert_eq!(
            store
                .bridge_runtime_status(BridgePlatform::Telegram)
                .unwrap()
                .unwrap()
                .cursor,
            future
        );
        assert!(decode_cursor("not-json", "7", "-1001").is_err());
        assert!(decode_cursor(&"x".repeat(3_000), "7", "-1001").is_err());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn username_chat_filters_posts_and_binds_by_checked_numeric_id() {
        let (store, path) = store();
        let mut rt = runtime();
        rt.chat_id = "@team_channel".to_string();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut transport = FakeTransport::new();
        transport.chat = Ok(json!({"ok":true,"result":{"id":-1001,"username":"team_channel"}}));
        assert_eq!(checked_chat_scope(&rt, &mut transport).unwrap(), "-1001");
        let mut cursor = set_cursor(&store, &owner, &bot.id, "-1001", 30);
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":31,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/help"}
        }]})));
        let mut dispatch = |_: &BotCommand, _: Option<&str>| BotDispatchOutcome::terminal("help");
        poll_once_scoped(
            &store,
            &rt,
            &bot,
            "-1001",
            &owner,
            &mut cursor,
            &mut transport,
            &mut dispatch,
        )
        .unwrap();
        assert_eq!(decode_cursor(&cursor, &bot.id, "-1001").unwrap(), Some(31));
        assert_eq!(transport.posted[0].0, "-1001");
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_bounds_identity_without_network() {
        let rt = runtime();
        let mut transport = FakeTransport::new();
        let checked = check_with_transport(&rt, &mut transport).unwrap();
        assert_eq!(checked.external_identity.as_deref(), Some("7"));
        assert_eq!(checked.external_scope.as_deref(), Some("-1001"));

        transport.me = Ok(json!({"ok":true,"result":{"id":8,"username":"x\nsecret"}}));
        let error = check_with_transport(&rt, &mut transport).unwrap_err();
        assert_eq!(error.class, "invalid_response");
        assert!(!error.message.contains("secret"));

        let mut mixed_case = rt.clone();
        mixed_case.bot_username = Some("WEAVE_BOT".to_string());
        transport.me = Ok(json!({"ok":true,"result":{"id":9,"username":"weave_bot"}}));
        check_with_transport(&mixed_case, &mut transport).unwrap();

        transport.me = Ok(json!({"ok":true,"result":{"id":10,"username":"OTHER_SENTINEL"}}));
        let error = check_with_transport(&rt, &mut transport).unwrap_err();
        assert_eq!(error.class, "identity_mismatch");
        assert!(!error.message.contains("OTHER_SENTINEL"));
        assert!(!error.message.contains("weave_bot"));

        transport.me = Ok(json!({"ok":true,"result":{"id":7,"username":"weave_bot"}}));
        transport.chat = Ok(json!({"ok":true,"result":{"id":-2002}}));
        let error = check_with_transport(&rt, &mut transport).unwrap_err();
        assert_eq!(error.class, "scope_mismatch");
        assert!(!error.message.contains("2002"));

        let mut username_chat = rt.clone();
        username_chat.chat_id = "@team_channel".to_string();
        transport.chat = Ok(json!({
            "ok":true,
            "result":{"id":-3003,"username":"TEAM_CHANNEL"}
        }));
        check_with_transport(&username_chat, &mut transport).unwrap();
        transport.chat = Ok(json!({
            "ok":true,
            "result":{"id":-3003,"username":"different_channel"}
        }));
        assert_eq!(
            check_with_transport(&username_chat, &mut transport)
                .unwrap_err()
                .class,
            "scope_mismatch"
        );

        transport.chat = Err(local_error(
            "api_rejected",
            "Telegram API rejected the request",
        ));
        let error = check_with_transport(&rt, &mut transport).unwrap_err();
        assert_eq!(error.class, "api_rejected");
        assert!(!error.message.contains(&rt.chat_id));

        transport.chat = Ok(json!({"ok":true,"result":{}}));
        let error = check_with_transport(&rt, &mut transport).unwrap_err();
        assert_eq!(error.class, "invalid_response");
    }

    #[test]
    fn transport_construction_failure_happens_before_runtime_claim() {
        let (store, path) = store();
        let config = Config {
            telegram_token: Some("test-token".to_string()),
            telegram_chat_id: Some("-1001".to_string()),
            telegram_identity: Some("telegram".to_string()),
            telegram_recipient: Some("worker".to_string()),
            telegram_bot_username: Some("weave_bot".to_string()),
            ..Config::default()
        };
        let error = run_with_transport_factory::<FakeTransport, _>(&store, &config, |_| {
            Err(anyhow::anyhow!("synthetic transport construction failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("synthetic transport"));
        assert!(store
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .is_none());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wrong_chat_is_ignored_but_cursor_advances() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 10);
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":11,
            "message":{"chat":{"id":-999},"from":{"username":"mallory"},"text":"ignore"}
        }]})));
        poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap();
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(11)
        );
        assert!(store
            .inbox("worker", false, false, 10)
            .unwrap()
            .0
            .is_empty());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reclaimed_owner_cannot_commit_ordinary_inbound_message() {
        let (store, path) = store();
        let rt = runtime();
        let stale_owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &stale_owner, &bot.id, &rt.chat_id, 10);
        let replacement = "replacement-owner";
        assert!(store
            .claim_bridge_runtime(
                BridgePlatform::Telegram,
                &rt.identity,
                &rt.recipient,
                replacement,
                Some(2),
                "test-host",
                i64::MAX,
            )
            .unwrap()
            .is_some());

        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":11,
            "message":{
                "chat":{"id":-1001},
                "from":{"username":"alice"},
                "text":"must not commit"
            }
        }]})));
        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &stale_owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert!(store
            .inbox("worker", false, false, 10)
            .unwrap()
            .0
            .is_empty());
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(10)
        );

        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_outbound_post_keeps_message_unread() {
        let (store, path) = store();
        let rt = runtime();
        store
            .send("agent", &rt.identity, None, "pending", None, None)
            .unwrap();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 10);
        let mut transport = FakeTransport::new();
        transport
            .updates
            .push_back(Ok(json!({"ok":true,"result":[]})));
        transport
            .posts
            .push_back(Err(BridgeCallError::new("transport", "failed")));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .is_err());
        assert_eq!(
            store
                .peek_oldest_unread(&rt.identity)
                .unwrap()
                .unwrap()
                .body,
            "pending"
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ownership_loss_before_outbound_prevents_external_post() {
        let (store, path) = store();
        let rt = runtime();
        store
            .send("agent", &rt.identity, None, "pending", None, None)
            .unwrap();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 10);
        assert!(store
            .release_bridge_runtime(BridgePlatform::Telegram, &owner)
            .unwrap());
        let mut transport = FakeTransport::new();
        transport
            .updates
            .push_back(Ok(json!({"ok":true,"result":[]})));
        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert!(transport.posted.is_empty());
        assert_eq!(
            store
                .peek_oldest_unread(&rt.identity)
                .unwrap()
                .unwrap()
                .body,
            "pending"
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ownership_loss_before_mutating_command_prevents_mesh_write() {
        let _env_lock = crate::testenv::lock_env();
        let _writes = crate::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 10);
        assert!(store
            .release_bridge_runtime(BridgePlatform::Telegram, &owner)
            .unwrap());
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":11,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/send worker must-not-send"}
        }]})));
        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "ownership_lost");
        assert!(store
            .inbox("worker", false, false, 10)
            .unwrap()
            .0
            .is_empty());
        assert!(transport.posted.is_empty());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn api_rejection_and_failed_reply_do_not_advance_cursor() {
        let _env_lock = crate::testenv::lock_env();
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let initial = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 20);
        let mut cursor = initial.clone();
        let mut rejected = FakeTransport::new();
        rejected
            .updates
            .push_back(crate::bridge::validate_api_response(
                200,
                br#"{"ok":false,"description":"TOKEN_SENTINEL"}"#,
            ));
        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut rejected,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(cursor, initial);
        assert!(!format!("{} {}", error.class, error.message).contains("TOKEN_SENTINEL"));

        let mut failed_reply = FakeTransport::new();
        failed_reply
            .updates
            .push_back(Ok(json!({"ok":true,"result":[{
                "update_id":21,
                "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/help"}
            }]})));
        failed_reply.posts.push_back(Err(BridgeCallError::new(
            "api_rejected",
            "request rejected",
        )));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut failed_reply,
            &no_injector(),
        )
        .is_err());
        assert_eq!(cursor, initial);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mutating_command_is_not_reexecuted_when_response_post_fails() {
        let _env_lock = crate::testenv::lock_env();
        let _writes = crate::testenv::EnvVarGuard::set("WEAVE_BOT_WRITES", "1");
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 60);
        let update = json!({"ok":true,"result":[{
            "update_id":61,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/send worker once"}
        }]});
        let mut first = FakeTransport::new();
        first.updates.push_back(Ok(update.clone()));
        first.posts.push_back(Err(BridgeCallError::new(
            "transport",
            "response post failed",
        )));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut first,
            &no_injector(),
        )
        .is_err());
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(61)
        );
        assert_eq!(store.inbox("worker", false, false, 10).unwrap().0.len(), 1);

        let mut replay = FakeTransport::new();
        replay.updates.push_back(Ok(update));
        poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut replay,
            &no_injector(),
        )
        .unwrap();
        assert_eq!(store.inbox("worker", false, false, 10).unwrap().0.len(), 1);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn local_inbound_store_failure_does_not_advance_cursor() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 70);
        let initial = cursor.clone();
        let mut invalid_runtime = rt.clone();
        invalid_runtime.recipient = "bad\nrecipient".to_string();
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":71,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"hello"}
        }]})));
        let error = poll_once(
            &store,
            &Config::default(),
            &invalid_runtime,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "local_store");
        assert_eq!(cursor, initial);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn route_change_bootstraps_without_replaying_pending_update() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let mut cursor = set_cursor(&store, &owner, "7", &rt.chat_id, 500);
        let new_bot = bot("8");
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":900,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"old backlog"}
        }]})));
        let report = poll_once(
            &store,
            &Config::default(),
            &rt,
            &new_bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap();
        assert!(report.bootstrapped);
        assert_eq!(transport.update_requests, vec![(-1, 0)]);
        assert_eq!(
            decode_cursor(&cursor, &new_bot.id, &rt.chat_id).unwrap(),
            Some(900)
        );
        assert!(store
            .inbox("worker", false, false, 10)
            .unwrap()
            .0
            .is_empty());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_bootstrap_does_not_create_or_replace_cursor() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = String::new();
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Err(BridgeCallError::new(
            "api_rejected",
            "bridge API rejected the request",
        )));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .is_err());
        assert!(cursor.is_empty());
        assert!(store
            .bridge_runtime_status(BridgePlatform::Telegram)
            .unwrap()
            .unwrap()
            .cursor
            .is_empty());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn chat_scope_change_resets_cursor_and_bootstraps() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, "-2002", 700);
        let mut transport = FakeTransport::new();
        transport
            .updates
            .push_back(Ok(json!({"ok":true,"result":[]})));
        let report = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap();
        assert!(report.bootstrapped);
        assert_eq!(transport.update_requests, vec![(-1, 0)]);
        assert_eq!(
            decode_cursor(&cursor, &bot.id, &rt.chat_id).unwrap(),
            Some(0)
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn inbound_replay_is_idempotent_and_attributed_from_bridge() {
        let (store, path) = store();
        let rt = runtime();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let update = json!({"ok":true,"result":[{
            "update_id":31,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"hello"}
        }]});
        for _ in 0..2 {
            let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 30);
            let mut transport = FakeTransport::new();
            transport.updates.push_back(Ok(update.clone()));
            poll_once(
                &store,
                &Config::default(),
                &rt,
                &bot,
                &owner,
                &mut cursor,
                &mut transport,
                &no_injector(),
            )
            .unwrap();
        }
        let rows = store.inbox("worker", false, false, 10).unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "telegram");
        assert_eq!(rows[0].body, "[telegram:alice] hello");
        assert!(rows[0].idempotency_key.is_some());
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_inbox_command_reply_leaves_exact_rows_unread() {
        let (store, path) = store();
        let rt = runtime();
        store
            .send(
                "agent",
                &rt.identity,
                None,
                "SHOULD-STAY-UNREAD",
                None,
                None,
            )
            .unwrap();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 40);
        let initial = cursor.clone();
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":41,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/inbox"}
        }]})));
        transport
            .posts
            .push_back(Err(BridgeCallError::new("transport", "failed")));
        assert!(poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .is_err());
        assert_eq!(cursor, initial);
        assert_eq!(
            store
                .peek_oldest_unread(&rt.identity)
                .unwrap()
                .unwrap()
                .body,
            "SHOULD-STAY-UNREAD"
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accepted_inbox_reply_with_local_ack_failure_rolls_back_cursor_and_receipts() {
        let (store, path) = store();
        let rt = runtime();
        let message_id = store
            .send("agent", &rt.identity, None, "accepted remotely", None, None)
            .unwrap();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 45);
        let initial = cursor.clone();
        let mut inner = FakeTransport::new();
        inner.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":46,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/inbox"}
        }]})));
        let mut transport = ExpireOnAcceptedPost {
            inner,
            store: &store,
            message_id,
        };

        let error = poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap_err();
        assert_eq!(error.class, "local_ack");
        assert_eq!(transport.inner.posted.len(), 1, "provider accepted once");
        assert_eq!(cursor, initial);
        assert_eq!(
            store
                .bridge_runtime_status(BridgePlatform::Telegram)
                .unwrap()
                .unwrap()
                .cursor,
            initial
        );
        assert!(store.receipts(message_id).unwrap().is_empty());
        drop(transport);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accepted_inbox_reply_acks_only_selected_snapshot_rows() {
        let (store, path) = store();
        let rt = runtime();
        store
            .send("older-agent", &rt.identity, None, "older", None, None)
            .unwrap();
        store
            .send(
                "newer-agent",
                &rt.identity,
                None,
                &"x".repeat(MAX_BRIDGE_TEXT_CHARS * 2),
                None,
                None,
            )
            .unwrap();
        let owner = claim(&store, &rt);
        let bot = bot("7");
        let mut cursor = set_cursor(&store, &owner, &bot.id, &rt.chat_id, 50);
        let mut transport = FakeTransport::new();
        transport.updates.push_back(Ok(json!({"ok":true,"result":[{
            "update_id":51,
            "message":{"chat":{"id":-1001},"from":{"username":"alice"},"text":"/inbox"}
        }]})));
        poll_once(
            &store,
            &Config::default(),
            &rt,
            &bot,
            &owner,
            &mut cursor,
            &mut transport,
            &no_injector(),
        )
        .unwrap();
        assert_eq!(transport.posted.len(), 1);
        assert!(transport.posted[0].1.chars().count() <= MAX_BRIDGE_TEXT_CHARS);
        assert!(transport.posted[0].1.contains("1 more unread"));
        let remaining = store.peek_oldest_unread(&rt.identity).unwrap().unwrap();
        assert_eq!(remaining.sender, "newer-agent");
        assert!(remaining.body.len() > MAX_BRIDGE_TEXT_CHARS);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn body_cap_preserves_multibyte_boundary() {
        let body = bounded_body("telegram", "alice", &"é".repeat(MAX_BODY));
        assert!(body.len() <= MAX_BODY);
        assert!(body.is_char_boundary(body.len()));
        assert!(body.starts_with("[telegram:alice] "));
    }
}
