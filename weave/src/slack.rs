//! WL-048 / ADR-0004: Slack bridge (`weave slack`).
//!
//! A poll-only v1 bridge between a Slack channel and the weave mesh, symmetric to
//! the Telegram bridge:
//! - **inbound** — polls `conversations.history`; each human message becomes a
//!   `Store::send` from the configured bridge identity into the mesh;
//! - **outbound** — polls the bridge identity's weave inbox and relays new
//!   messages to the channel via `chat.postMessage`.
//!
//! Invariants are identical to `telegram.rs`: NO shell, the bot token is a SECRET
//! (config/env, never logged, Bearer header only — never a logged URL), inbound
//! bodies/idents pass weave's caps before `Store::send`, and the HTTP client is
//! the shared `reqwest::blocking` (rustls) client. Pure builders/parsers below.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use weave_core::config::Config;
use weave_core::store::{check_body, check_ident, Store, MAX_BODY};

use crate::telegram::{dispatch_bot_command, parse_bot_command, sanitize_inbound_ident};

const DEFAULT_BRIDGE_IDENTITY: &str = "slack";
const REQUEST_TIMEOUT_SECS: u64 = 30;
const POLL_SECS: u64 = 5;

/// Build the JSON body for Slack `chat.postMessage`. Pure — no network.
pub fn slack_post_payload(channel: &str, text: &str) -> Value {
    json!({ "channel": channel, "text": text })
}

/// Build the query map for `conversations.history` from a cursor timestamp. Pure.
/// `oldest` is the Slack `ts` watermark ("0" to read from the start).
pub fn slack_history_payload(channel: &str, oldest: &str) -> Value {
    json!({ "channel": channel, "oldest": oldest, "limit": 50 })
}

/// Parse one Slack `message` object from a `conversations.history` response into
/// `(from, text)`. Returns `None` for non-user messages (bot echoes, joins,
/// missing fields). Pure. The caller sanitizes `from` before `send`.
pub fn parse_slack_message(msg: &Value) -> Option<(String, String)> {
    // Skip messages with a subtype (joins, bot messages, edits) and our own bot.
    if msg.get("subtype").is_some() || msg.get("bot_id").is_some() {
        return None;
    }
    let text = msg.get("text")?.as_str()?.to_string();
    if text.is_empty() {
        return None;
    }
    let from = msg
        .get("user")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "slack-user".to_string());
    Some((from, text))
}

/// Extract the latest message `ts` (watermark) from a history response, if any.
/// Pure. Slack returns newest-first; we want the max `ts` to advance the cursor.
pub fn latest_ts(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .filter_map(|m| m.get("ts").and_then(|t| t.as_str()))
        .max_by(|a, b| {
            a.parse::<f64>()
                .unwrap_or(0.0)
                .partial_cmp(&b.parse::<f64>().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|s| s.to_string())
}

fn resolve_token(config: &Config) -> Option<String> {
    config
        .slack_token
        .clone()
        .or_else(|| std::env::var("WEAVE_SLACK_TOKEN").ok())
        .filter(|s| !s.is_empty())
}

fn resolve_channel(config: &Config) -> Option<String> {
    config
        .slack_channel
        .clone()
        .or_else(|| std::env::var("WEAVE_SLACK_CHANNEL").ok())
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

/// Run the Slack bridge blocking loop on the calling thread.
pub fn run(store: &dyn Store, config: &Config) -> Result<()> {
    let token = resolve_token(config).context(
        "Slack bot token not configured (set slack_token in config or WEAVE_SLACK_TOKEN)",
    )?;
    let channel = resolve_channel(config)
        .context("Slack channel not configured (set slack_channel or WEAVE_SLACK_CHANNEL)")?;
    let identity = resolve_identity(config);
    let recipient = identity.clone();

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .context("building reqwest client")?;

    eprintln!("[weave-slack] bridge started as identity '{identity}'");

    // WL-073: Slack command replies dispatch through the SAME handler as
    // MCP/CLI/Telegram. Read commands are safe; write commands require
    // WEAVE_BOT_WRITES=1 and then pass the dangerous-tool gate explicitly.
    let injector = crate::RealInjector {
        preferred_mux: crate::parse_mux_preference(config),
    };

    // Start the cursor at "now-ish": skip channel history on first run by reading
    // the latest ts and using it as the initial watermark.
    let mut oldest = String::from("0");
    let mut first_pass = true;

    loop {
        // --- inbound: poll conversations.history ---
        let hist = slack_history_payload(&channel, &oldest);
        match client
            .get("https://slack.com/api/conversations.history")
            .header("Authorization", format!("Bearer {token}"))
            .query(&[
                ("channel", channel.as_str()),
                ("oldest", oldest.as_str()),
                ("limit", "50"),
            ])
            .json(&hist)
            .send()
        {
            Ok(resp) => {
                if let Ok(v) = resp.json::<Value>() {
                    if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
                        if let Some(ts) = latest_ts(arr) {
                            oldest = ts;
                        }
                        if !first_pass {
                            for m in arr {
                                if let Some((from, text)) = parse_slack_message(m) {
                                    if let Some(cmd) = parse_bot_command(&text) {
                                        let reply = dispatch_bot_command(
                                            store, config, &identity, &cmd, &injector,
                                        );
                                        let payload = slack_post_payload(&channel, &reply);
                                        if let Err(e) = client
                                            .post("https://slack.com/api/chat.postMessage")
                                            .header("Authorization", format!("Bearer {token}"))
                                            .json(&payload)
                                            .send()
                                        {
                                            eprintln!(
                                                "[weave-slack] command reply error: {}",
                                                crate::telegram::redact_reqwest_error(&e)
                                            );
                                        }
                                    } else {
                                        relay_inbound(store, &from, &recipient, &text, &identity);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => eprintln!(
                "[weave-slack] conversations.history error: {}",
                crate::telegram::redact_reqwest_error(&e)
            ),
        }
        first_pass = false;

        // --- outbound: relay new weave messages addressed to the bridge ---
        if let Ok((msgs, _)) = store.inbox(&identity, false, true, 50) {
            for m in msgs {
                let text = format!("[{}] {}", m.sender, m.body);
                let payload = slack_post_payload(&channel, &text);
                if let Err(e) = client
                    .post("https://slack.com/api/chat.postMessage")
                    .header("Authorization", format!("Bearer {token}"))
                    .json(&payload)
                    .send()
                {
                    eprintln!(
                        "[weave-slack] chat.postMessage error: {}",
                        crate::telegram::redact_reqwest_error(&e)
                    );
                }
            }
        }

        std::thread::sleep(Duration::from_secs(POLL_SECS));
    }
}

/// Sanitize + cap an inbound message and `send` it into the mesh (same discipline
/// as the Telegram bridge).
fn relay_inbound(store: &dyn Store, raw_from: &str, recipient: &str, text: &str, fallback: &str) {
    let from = sanitize_inbound_ident(raw_from, fallback);
    if check_ident("from", &from).is_err() {
        eprintln!("[weave-slack] dropping message: invalid sender ident");
        return;
    }
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
        eprintln!("[weave-slack] dropping message: body too long");
        return;
    }
    if let Err(e) = store.send(&from, recipient, None, body, None, None) {
        eprintln!("[weave-slack] send error: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_payload_shape() {
        let p = slack_post_payload("C123", "hello");
        assert_eq!(p["channel"], "C123");
        assert_eq!(p["text"], "hello");
    }

    #[test]
    fn history_payload_shape() {
        let p = slack_history_payload("C123", "1700000000.000100");
        assert_eq!(p["channel"], "C123");
        assert_eq!(p["oldest"], "1700000000.000100");
        assert_eq!(p["limit"], 50);
    }

    #[test]
    fn parse_well_formed_message() {
        let m = json!({ "type": "message", "user": "U1", "text": "hi", "ts": "1.0" });
        assert_eq!(
            parse_slack_message(&m),
            Some(("U1".to_string(), "hi".to_string()))
        );
    }

    #[test]
    fn parse_skips_bot_and_subtype_and_empty() {
        let bot = json!({ "user": "U1", "text": "hi", "bot_id": "B1" });
        assert_eq!(parse_slack_message(&bot), None);
        let join = json!({ "subtype": "channel_join", "user": "U1", "text": "x" });
        assert_eq!(parse_slack_message(&join), None);
        let empty = json!({ "user": "U1", "text": "" });
        assert_eq!(parse_slack_message(&empty), None);
        let no_text = json!({ "user": "U1" });
        assert_eq!(parse_slack_message(&no_text), None);
    }

    #[test]
    fn latest_ts_picks_max() {
        let msgs = vec![
            json!({ "ts": "1700000000.000100" }),
            json!({ "ts": "1700000005.000100" }),
            json!({ "ts": "1699999999.000100" }),
        ];
        assert_eq!(latest_ts(&msgs).as_deref(), Some("1700000005.000100"));
        assert_eq!(latest_ts(&[]), None);
    }

    #[test]
    fn slack_reuses_bot_command_grammar_for_reads_and_writes() {
        use crate::telegram::{bot_command_rpc, parse_bot_command, BotCommand};

        assert_eq!(parse_bot_command("/inbox"), Some(BotCommand::Inbox));
        assert_eq!(parse_bot_command("/peers"), Some(BotCommand::Peers));
        assert_eq!(parse_bot_command("/sessions"), Some(BotCommand::Sessions));
        assert_eq!(
            parse_bot_command("/send worker run the check"),
            Some(BotCommand::Send {
                to: "worker".to_string(),
                body: "run the check".to_string()
            })
        );
        assert_eq!(
            parse_bot_command("/ask worker ship it?"),
            Some(BotCommand::Ask {
                to: "worker".to_string(),
                body: "ship it?".to_string()
            })
        );
        assert_eq!(
            parse_bot_command("/answer ask_1_2 done"),
            Some(BotCommand::Answer {
                id: "ask_1_2".to_string(),
                body: "done".to_string()
            })
        );
        assert_eq!(
            parse_bot_command("/reply 42 ack"),
            Some(BotCommand::Reply {
                message_id: 42,
                body: "ack".to_string()
            })
        );

        let gated = bot_command_rpc(
            &BotCommand::Send {
                to: "worker".to_string(),
                body: "run".to_string(),
            },
            "slack",
            false,
        );
        assert!(gated.is_err(), "write commands must be explicitly gated");

        let rpc = bot_command_rpc(
            &BotCommand::Send {
                to: "worker".to_string(),
                body: "run".to_string(),
            },
            "slack",
            true,
        )
        .expect("writes enabled")
        .expect("send rpc");
        assert_eq!(rpc["params"]["name"], "weave_send");
        assert_eq!(rpc["params"]["arguments"]["from"], "slack");
        assert_eq!(rpc["params"]["arguments"]["to"], "worker");
        assert_eq!(rpc["params"]["arguments"]["body"], "run");
    }
}
