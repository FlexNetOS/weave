//! WL-048 / ADR-0004: Telegram bridge (`weave telegram`).
//!
//! A poll-only v1 bridge between a Telegram chat and the weave mesh:
//! - **inbound** — long-polls Telegram `getUpdates`; each human message becomes a
//!   `Store::send` from the configured bridge identity into the mesh;
//! - **outbound** — polls the bridge identity's weave inbox and relays new
//!   messages to the chat via `sendMessage`.
//!
//! Invariants: NO shell (the bridge spawns nothing); the bot token is a SECRET
//! (read from config/env, never logged, never placed in a logged URL); inbound
//! bodies/idents pass weave's input caps before `Store::send`; the HTTP client is
//! the SAME `reqwest::blocking` (rustls) client `llm` uses (shared via the
//! `surfaces`/`llm` feature union). The payload-builders and inbound-parser are
//! pure (no network) so they unit-test directly.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use weave_core::config::Config;
use weave_core::store::{check_body, check_ident, Store, MAX_BODY};

/// Default weave identity a Telegram-originated message is attributed to when the
/// operator does not set `bridge_identity` / `WEAVE_BRIDGE_IDENTITY`.
const DEFAULT_BRIDGE_IDENTITY: &str = "telegram";
/// Telegram long-poll timeout (seconds). Bounded; the reqwest client timeout is
/// set slightly higher so the long-poll can return naturally.
const POLL_TIMEOUT_SECS: u64 = 25;
/// reqwest per-request timeout (mirrors llm's 30s ceiling), above the long-poll.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// How often the outbound relay checks the bridge inbox.
const OUTBOUND_POLL_SECS: u64 = 5;

/// Build the JSON body for Telegram `sendMessage`. Pure — no network. `text` is
/// the message body to deliver to the chat.
pub fn telegram_send_payload(chat_id: &str, text: &str) -> Value {
    json!({ "chat_id": chat_id, "text": text })
}

/// Build the JSON body for `getUpdates` long-poll from the last-seen update id.
/// Pure. `offset` is `last_update_id + 1` (Telegram's ack convention).
pub fn telegram_get_updates_payload(offset: i64, timeout_secs: u64) -> Value {
    json!({ "offset": offset, "timeout": timeout_secs })
}

/// Parse one Telegram `update` object into `(from, text)` — the sender's display
/// handle and the message text — or `None` when the update carries no usable text
/// message (edits, joins, missing fields). Pure. The returned `from` is the raw
/// handle; the caller sanitizes it to a valid weave ident before `send`.
pub fn parse_telegram_update(update: &Value) -> Option<(String, String)> {
    let msg = update.get("message")?;
    let text = msg.get("text")?.as_str()?.to_string();
    if text.is_empty() {
        return None;
    }
    let from = msg
        .get("from")
        .and_then(|f| {
            f.get("username")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    f.get("first_name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| "telegram-user".to_string());
    Some((from, text))
}

/// Sanitize a raw inbound handle into a weave ident: keep `[A-Za-z0-9._-]`, lower
/// nothing, bound length, and fall back to a safe default when nothing usable
/// remains. The result is re-checked with `check_ident` by the caller. Pure.
pub fn sanitize_inbound_ident(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Resolve the bot token: config field first, then `WEAVE_TELEGRAM_TOKEN`
/// (mirrors llm's precedence). The value is a secret — never logged here.
fn resolve_token(config: &Config) -> Option<String> {
    config
        .telegram_token
        .clone()
        .or_else(|| std::env::var("WEAVE_TELEGRAM_TOKEN").ok())
        .filter(|s| !s.is_empty())
}

fn resolve_chat_id(config: &Config) -> Option<String> {
    config
        .telegram_chat_id
        .clone()
        .or_else(|| std::env::var("WEAVE_TELEGRAM_CHAT_ID").ok())
        .filter(|s| !s.is_empty())
}

fn resolve_identity(config: &Config) -> String {
    config
        .bridge_identity
        .clone()
        .or_else(|| std::env::var("WEAVE_BRIDGE_IDENTITY").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BRIDGE_IDENTITY.to_string())
}

/// Render a `reqwest::Error` into a **token-free** one-line summary safe to log.
///
/// SECURITY: the Telegram API embeds the bot token in the request URL
/// (`https://api.telegram.org/bot<TOKEN>/getUpdates`), and a `reqwest::Error`'s
/// `Display` includes that URL — so logging the raw `{e}` would leak the secret to
/// stderr on any transient HTTP failure. We therefore log only the error *class*
/// (timeout / connect / status / decode) and the HTTP status, never the URL.
pub fn redact_reqwest_error(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else if e.is_decode() {
        "decode failed"
    } else if e.is_request() {
        "request failed"
    } else {
        "http error"
    };
    match e.status() {
        Some(s) => format!("{kind} (status {})", s.as_u16()),
        None => kind.to_string(),
    }
}

/// WL-052b: a structured bot command parsed from an inbound `/…` message. Read-only
/// reads are always enabled; writes require the explicit bot-write gate.
#[derive(Debug, PartialEq, Eq)]
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

/// Parse an inbound message into a [`BotCommand`]. Returns `None` for ordinary text
/// (no leading `/`), which falls through to the relay path. An unknown `/x` maps to
/// `Help`. The Telegram group suffix form (`/inbox@mybot`) is tolerated.
pub fn parse_bot_command(text: &str) -> Option<BotCommand> {
    let first = text.split_whitespace().next().unwrap_or("");
    let rest = first.strip_prefix('/')?;
    let cmd = rest.split('@').next().unwrap_or(rest);
    Some(match cmd {
        "inbox" => BotCommand::Inbox,
        "peers" => BotCommand::Peers,
        "sessions" => BotCommand::Sessions,
        "send" => {
            let Some((to, body)) = parse_two_arg_command(text) else {
                return Some(BotCommand::Help);
            };
            BotCommand::Send { to, body }
        }
        "ask" => {
            let Some((to, body)) = parse_two_arg_command(text) else {
                return Some(BotCommand::Help);
            };
            BotCommand::Ask { to, body }
        }
        "answer" => {
            let Some((id, body)) = parse_two_arg_command(text) else {
                return Some(BotCommand::Help);
            };
            BotCommand::Answer { id, body }
        }
        "reply" => {
            let Some((id, body)) = parse_two_arg_command(text) else {
                return Some(BotCommand::Help);
            };
            let Ok(message_id) = id.parse::<i64>() else {
                return Some(BotCommand::Help);
            };
            BotCommand::Reply { message_id, body }
        }
        _ => BotCommand::Help,
    })
}

fn parse_two_arg_command(text: &str) -> Option<(String, String)> {
    let mut parts = text.splitn(3, char::is_whitespace);
    let _cmd = parts.next()?;
    let first = parts.next()?.trim();
    let rest = parts.next()?.trim();
    if first.is_empty() || rest.is_empty() {
        return None;
    }
    Some((first.to_string(), rest.to_string()))
}

/// Bot write commands are explicit opt-in. Human chat writes route through the
/// dangerous-tool gate (`dispatch_request(..., dangerous=true)`) only when this
/// returns true.
pub fn bot_write_commands_enabled(_config: &Config) -> bool {
    matches!(
        std::env::var("WEAVE_BOT_WRITES")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Map a command to the JSON-RPC `tools/call` it dispatches through — the SAME
/// `dispatch_request` → `call_tool` handler the MCP and CLI surfaces use (the WL-052
/// one-handler-many-surfaces law). `Help` has no RPC (handled locally). Write ops
/// return an error until the explicit bot-write gate is enabled.
pub fn bot_command_rpc(
    cmd: &BotCommand,
    me: &str,
    allow_writes: bool,
) -> std::result::Result<Option<Value>, String> {
    let (name, args) = match cmd {
        BotCommand::Help => return Ok(None),
        BotCommand::Inbox => ("weave_inbox", json!({"me": me, "include_read": false})),
        BotCommand::Peers => ("weave_peers", json!({"circle": "*"})),
        BotCommand::Sessions => ("weave_sessions", json!({"circle": "*"})),
        BotCommand::Send { to, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            ("weave_send", json!({"from": me, "to": to, "body": body}))
        }
        BotCommand::Ask { to, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            ("weave_ask", json!({"from": me, "to": to, "body": body}))
        }
        BotCommand::Answer { id, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            ("weave_answer", json!({"from": me, "id": id, "body": body}))
        }
        BotCommand::Reply { message_id, body } => {
            if !allow_writes {
                return Err(bot_writes_disabled_text());
            }
            (
                "weave_reply",
                json!({"from": me, "message_id": message_id, "body": body}),
            )
        }
    };
    Ok(Some(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })))
}

fn bot_writes_disabled_text() -> String {
    "bot write commands are disabled; set WEAVE_BOT_WRITES=1 to enable /send, /ask, /answer, and /reply".to_string()
}

/// The `/help` listing.
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

/// Extract the human-readable reply from a JSON-RPC `tools/call` response
/// (`result.content[0].text`), or an error message, falling back to the raw string.
pub fn format_bot_reply(resp: Option<&str>) -> String {
    let raw = match resp {
        Some(r) => r,
        None => return "(no response)".to_string(),
    };
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    if let Some(t) = v.pointer("/result/content/0/text").and_then(|x| x.as_str()) {
        return t.to_string();
    }
    if let Some(e) = v.pointer("/error/message").and_then(|x| x.as_str()) {
        return format!("error: {e}");
    }
    raw.to_string()
}

pub fn dispatch_bot_command(
    store: &dyn Store,
    config: &Config,
    identity: &str,
    cmd: &BotCommand,
    injector: &dyn weave_inject::Injector,
) -> String {
    let allow_writes = bot_write_commands_enabled(config);
    match bot_command_rpc(cmd, identity, allow_writes) {
        Ok(None) => bot_help_text(),
        Err(e) => e,
        Ok(Some(rpc)) => {
            let resp = weave_mcp::mcp::dispatch_request(
                store,
                &Some(identity.to_string()),
                None,
                &[],
                &weave_mcp::PullConsent::empty(),
                &rpc,
                injector,
                allow_writes,
            );
            format_bot_reply(resp.as_deref())
        }
    }
}

/// Run the Telegram bridge blocking loop on the calling thread (like `Cmd::Serve`).
/// Returns an error only on fatal misconfiguration; transient HTTP errors are
/// logged to stderr and retried. WL-052b: inbound `/commands` are answered via the
/// shared `dispatch_request` handler; ordinary text relays into the mesh.
pub fn run(store: &dyn Store, config: &Config) -> Result<()> {
    let token = resolve_token(config).context(
        "Telegram bot token not configured (set telegram_token in config or WEAVE_TELEGRAM_TOKEN)",
    )?;
    let chat_id = resolve_chat_id(config);
    let identity = resolve_identity(config);
    // The bridge posts inbound human messages to this weave recipient. Default to
    // a broadcast-free direct recipient: the bridge identity itself acts as a
    // mailbox the mesh can read; operators route from there.
    let recipient = identity.clone();

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .context("building reqwest client")?;

    let base = format!("https://api.telegram.org/bot{token}");
    eprintln!("[weave-telegram] bridge started as identity '{identity}'");

    let mut offset: i64 = 0;

    // WL-052b: command replies dispatch through the SAME handler as MCP/CLI, so the
    // bot needs the real injector (read-only commands don't use it, but the shared
    // entry point requires it — and a future write command would nudge correctly).
    let injector = crate::RealInjector {
        preferred_mux: crate::parse_mux_preference(config),
    };

    loop {
        // --- inbound: long-poll getUpdates ---
        let updates_body = telegram_get_updates_payload(offset, POLL_TIMEOUT_SECS);
        match client
            .post(format!("{base}/getUpdates"))
            .json(&updates_body)
            .send()
        {
            Ok(resp) => {
                if let Ok(v) = resp.json::<Value>() {
                    if let Some(arr) = v.get("result").and_then(|r| r.as_array()) {
                        for update in arr {
                            if let Some(uid) = update.get("update_id").and_then(|i| i.as_i64()) {
                                offset = offset.max(uid + 1);
                            }
                            if let Some((from, text)) = parse_telegram_update(update) {
                                // WL-052b: a `/command` is answered structurally via the
                                // shared handler; ordinary text falls through to the relay.
                                if let Some(cmd) = parse_bot_command(&text) {
                                    let reply = dispatch_bot_command(
                                        store, config, &identity, &cmd, &injector,
                                    );
                                    if let Some(chat) = &chat_id {
                                        let payload = telegram_send_payload(chat, &reply);
                                        if let Err(e) = client
                                            .post(format!("{base}/sendMessage"))
                                            .json(&payload)
                                            .send()
                                        {
                                            eprintln!(
                                                "[weave-telegram] reply error: {}",
                                                redact_reqwest_error(&e)
                                            );
                                        }
                                    }
                                } else {
                                    relay_inbound(store, &from, &recipient, &text, &identity);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[weave-telegram] getUpdates error: {}",
                    redact_reqwest_error(&e)
                )
            }
        }

        // --- outbound: relay new weave messages addressed to the bridge ---
        // `inbox(mark_read=true)` returns only UNREAD messages and marks them read,
        // so each poll naturally yields exactly the new ones (no manual watermark).
        if let Some(chat) = &chat_id {
            if let Ok((msgs, _)) = store.inbox(&identity, false, true, 50) {
                for m in msgs {
                    let text = format!("[{}] {}", m.sender, m.body);
                    let payload = telegram_send_payload(chat, &text);
                    if let Err(e) = client
                        .post(format!("{base}/sendMessage"))
                        .json(&payload)
                        .send()
                    {
                        eprintln!(
                            "[weave-telegram] sendMessage error: {}",
                            redact_reqwest_error(&e)
                        );
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_secs(OUTBOUND_POLL_SECS));
    }
}

/// Sanitize + cap an inbound message and `send` it into the mesh. Caps are
/// enforced (ident + body) before the store write; a rejected message is dropped
/// with a stderr note (never a panic, never the token in the log).
fn relay_inbound(store: &dyn Store, raw_from: &str, recipient: &str, text: &str, fallback: &str) {
    let from = sanitize_inbound_ident(raw_from, fallback);
    if check_ident("from", &from).is_err() {
        eprintln!("[weave-telegram] dropping message: invalid sender ident");
        return;
    }
    // Cap the body on a UTF-8 boundary before the store check.
    let body = if text.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        &text[..cut]
    } else {
        text
    };
    if check_body(body).is_err() {
        eprintln!("[weave-telegram] dropping message: body too long");
        return;
    }
    if let Err(e) = store.send(&from, recipient, None, body, None, None) {
        eprintln!("[weave-telegram] send error: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_payload_shape() {
        let p = telegram_send_payload("123", "hello");
        assert_eq!(p["chat_id"], "123");
        assert_eq!(p["text"], "hello");
    }

    // ---- WL-052b bot command grammar ----------------------------------------

    #[test]
    fn parse_bot_command_recognizes_and_falls_through() {
        assert_eq!(parse_bot_command("/inbox"), Some(BotCommand::Inbox));
        assert_eq!(parse_bot_command("/peers"), Some(BotCommand::Peers));
        assert_eq!(parse_bot_command("/sessions"), Some(BotCommand::Sessions));
        assert_eq!(
            parse_bot_command("/send worker hello there"),
            Some(BotCommand::Send {
                to: "worker".into(),
                body: "hello there".into()
            })
        );
        assert_eq!(
            parse_bot_command("/ask worker ready?"),
            Some(BotCommand::Ask {
                to: "worker".into(),
                body: "ready?".into()
            })
        );
        assert_eq!(
            parse_bot_command("/answer ask_1_2 yes"),
            Some(BotCommand::Answer {
                id: "ask_1_2".into(),
                body: "yes".into()
            })
        );
        assert_eq!(
            parse_bot_command("/reply 42 got it"),
            Some(BotCommand::Reply {
                message_id: 42,
                body: "got it".into()
            })
        );
        assert_eq!(parse_bot_command("/send worker"), Some(BotCommand::Help));
        // group-suffix form + unknown -> Help.
        assert_eq!(
            parse_bot_command("/inbox@weavebot"),
            Some(BotCommand::Inbox)
        );
        assert_eq!(parse_bot_command("/wat"), Some(BotCommand::Help));
        assert_eq!(parse_bot_command("/help"), Some(BotCommand::Help));
        // ordinary text falls through to the relay (None).
        assert_eq!(parse_bot_command("hello there"), None);
        assert_eq!(parse_bot_command(""), None);
    }

    #[test]
    fn bot_command_rpc_maps_to_read_ops() {
        let rpc = bot_command_rpc(&BotCommand::Inbox, "bridge", false)
            .unwrap()
            .unwrap();
        assert_eq!(rpc["method"], "tools/call");
        assert_eq!(rpc["params"]["name"], "weave_inbox");
        assert_eq!(rpc["params"]["arguments"]["me"], "bridge");
        assert_eq!(
            bot_command_rpc(&BotCommand::Peers, "x", false)
                .unwrap()
                .unwrap()["params"]["name"],
            "weave_peers"
        );
        // Help has no RPC (answered locally).
        assert!(bot_command_rpc(&BotCommand::Help, "x", false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn bot_command_rpc_gates_write_ops() {
        let send = BotCommand::Send {
            to: "worker".into(),
            body: "hello".into(),
        };
        assert!(bot_command_rpc(&send, "bridge", false).is_err());
        let rpc = bot_command_rpc(&send, "bridge", true).unwrap().unwrap();
        assert_eq!(rpc["params"]["name"], "weave_send");
        assert_eq!(rpc["params"]["arguments"]["from"], "bridge");
        assert_eq!(rpc["params"]["arguments"]["to"], "worker");
        assert_eq!(rpc["params"]["arguments"]["body"], "hello");
    }

    #[test]
    fn format_bot_reply_extracts_text_or_error() {
        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"no unread"}],"isError":false}}"#;
        assert_eq!(format_bot_reply(Some(ok)), "no unread");
        let err = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"boom"}}"#;
        assert_eq!(format_bot_reply(Some(err)), "error: boom");
        assert_eq!(format_bot_reply(None), "(no response)");
    }

    #[test]
    fn get_updates_payload_shape() {
        let p = telegram_get_updates_payload(7, 25);
        assert_eq!(p["offset"], 7);
        assert_eq!(p["timeout"], 25);
    }

    #[test]
    fn parse_well_formed_update() {
        let u = json!({
            "update_id": 1,
            "message": { "text": "hi there", "from": { "username": "alice" } }
        });
        assert_eq!(
            parse_telegram_update(&u),
            Some(("alice".to_string(), "hi there".to_string()))
        );
    }

    #[test]
    fn parse_missing_text_returns_none() {
        let u = json!({ "update_id": 2, "message": { "from": { "username": "bob" } } });
        assert_eq!(parse_telegram_update(&u), None);
        let edit = json!({ "update_id": 3, "edited_message": { "text": "x" } });
        assert_eq!(parse_telegram_update(&edit), None);
    }

    #[test]
    fn parse_falls_back_when_no_username() {
        let u = json!({
            "update_id": 4,
            "message": { "text": "yo", "from": { "first_name": "Carol" } }
        });
        assert_eq!(
            parse_telegram_update(&u),
            Some(("Carol".to_string(), "yo".to_string()))
        );
    }

    #[test]
    fn ident_sanitization_strips_unsafe_chars() {
        assert_eq!(sanitize_inbound_ident("al ice!@#", "fb"), "alice");
        assert_eq!(sanitize_inbound_ident("a.b-c_d", "fb"), "a.b-c_d");
        assert_eq!(sanitize_inbound_ident("   ", "fb"), "fb");
        assert_eq!(sanitize_inbound_ident("<script>", "fb"), "script");
        // bounded to 64 chars
        let long = "x".repeat(200);
        assert_eq!(sanitize_inbound_ident(&long, "fb").len(), 64);
    }

    /// SECURITY: a real `reqwest::Error` produced from a URL that embeds the bot
    /// token must NOT leak the token when run through [`redact_reqwest_error`] —
    /// this is the exact string the bridge loop logs to stderr on a transient
    /// failure. (The raw `{e}` Display *does* contain the token; the redactor
    /// strips the URL.)
    #[test]
    fn error_log_never_contains_token() {
        let secret = "SECRET123TOKEN";
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        // Unresolvable host so .send() errors before any network leak; the token is
        // in the URL path exactly as the live bridge builds it.
        let base = format!("https://api.telegram.org.invalid/bot{secret}");
        let err = client
            .post(format!("{base}/getUpdates"))
            .json(&telegram_get_updates_payload(0, 1))
            .send()
            .expect_err("request to an invalid host must fail");
        // Sanity: confirm the raw Display WOULD have leaked (so the test is real).
        assert!(
            format!("{err}").contains(secret),
            "precondition: raw reqwest error should embed the URL/token"
        );
        // The redacted form the bridge actually logs must NOT contain the token.
        let redacted = redact_reqwest_error(&err);
        assert!(
            !redacted.contains(secret),
            "token leaked into the logged error string: {redacted}"
        );
        assert!(
            !redacted.contains("telegram.org"),
            "URL leaked into the logged error string: {redacted}"
        );
    }
}
