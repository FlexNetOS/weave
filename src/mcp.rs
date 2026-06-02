//! MCP stdio server: newline-delimited JSON-RPC 2.0 on stdin/stdout. Exposes
//! weave's messaging tools. On send, if the recipient is a registered injectable
//! peer, a live nudge is pushed into their pane via the native injector.
//!
//! stdout is reserved for protocol messages; all logging goes to stderr.

use crate::inject::{self, Target};
use crate::model::{self, fmt_ts};
use crate::store::{is_online, Store};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const SERVER_NAME: &str = "weave";
const SERVER_VERSION: &str = "0.1.0";
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn log(msg: &str) {
    eprintln!("[weave-mcp] {msg}");
}

/// Run the server loop until stdin closes. `me_default` seeds the identity for
/// tools when the caller omits `me`/`from` (e.g. from $WEAVE_SESSION).
pub fn run(store: &dyn Store, me_default: Option<String>) -> Result<()> {
    log(&format!(
        "starting; backend={} default_session={:?}",
        store.backend(),
        me_default
    ));
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("bad JSON: {e}"));
                continue;
            }
        };
        if let Some(resp) = handle(store, &me_default, &req) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    log("stdin closed; exiting");
    Ok(())
}

fn ident(args: &Value, key: &str, def: &Option<String>) -> Result<String, String> {
    if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            return Ok(s.trim().to_string());
        }
    }
    if let Some(d) = def {
        if !d.is_empty() {
            return Ok(d.clone());
        }
    }
    Err(format!(
        "'{key}' is required (no default session set). Pass e.g. \"{key}\": \"desktop\"."
    ))
}

fn handle(store: &dyn Store, me_default: &Option<String>, req: &Value) -> Option<String> {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = req.get("id").cloned();
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) get no reply.
    if id.is_none() {
        if method == "notifications/initialized" {
            log("client initialized");
        }
        return None;
    }
    let id = id.unwrap();

    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_PROTOCOL);
            let proto = if SUPPORTED.contains(&requested) {
                requested
            } else {
                DEFAULT_PROTOCOL
            };
            Some(reply(
                &id,
                json!({
                    "protocolVersion": proto,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                }),
            ))
        }
        "ping" => Some(reply(&id, json!({}))),
        "tools/list" => Some(reply(&id, json!({ "tools": tools() }))),
        "resources/list" => Some(reply(&id, json!({ "resources": [] }))),
        "prompts/list" => Some(reply(&id, json!({ "prompts": [] }))),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(store, me_default, name, &args) {
                Ok(text) => Some(reply(
                    &id,
                    json!({ "content": [{"type":"text","text": text}], "isError": false }),
                )),
                Err(e) => Some(reply(
                    &id,
                    json!({ "content": [{"type":"text","text": format!("Error: {e}")}], "isError": true }),
                )),
            }
        }
        _ => Some(reply_err(&id, -32601, &format!("Method not found: {method}"))),
    }
}

fn call_tool(
    store: &dyn Store,
    me_default: &Option<String>,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    match name {
        "weave_send" => tool_send(store, me_default, args),
        "weave_inbox" => tool_inbox(store, me_default, args),
        "weave_history" => tool_history(store, me_default, args),
        "weave_sessions" => tool_sessions(store, me_default, args),
        "weave_clear" => tool_clear(store, me_default, args),
        "weave_peers" => tool_peers(store),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn tool_send(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'to' is required (session name, or 'all' to broadcast).")?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mid = store.send(&from, to, subject, body).map_err(e)?;
    let dest = if model::is_broadcast(to) {
        "broadcast"
    } else {
        to
    };
    let mut out = format!("Sent message #{mid} from '{from}' to '{dest}'.");

    // Native push: nudge the recipient's pane if it's a registered injectable peer.
    if !model::is_broadcast(to) {
        if let Ok(Some(peer)) = store.get_peer(to) {
            let target = Target::from_peer(&peer);
            if target.injectable() {
                let nudge = format!("[weave] new message from {from} — run weave_inbox to read");
                match inject::inject(&target, &nudge) {
                    Ok(true) => out.push_str(&format!(
                        " Injected live nudge into {} target '{}'.",
                        target.mux.as_str(),
                        target.id
                    )),
                    Ok(false) => {}
                    Err(err) => out.push_str(&format!(
                        " (peer registered on {} but inject failed: {err}; it'll arrive on their next turn)",
                        target.mux.as_str()
                    )),
                }
            }
        }
    }
    Ok(out)
}

fn tool_inbox(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let include_read = args
        .get("include_read")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mark_read = match args.get("mark_read").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => !include_read,
    };
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);

    let (rows, remaining) = store.inbox(&me, include_read, mark_read, limit).map_err(e)?;
    if rows.is_empty() {
        let kind = if include_read { "messages" } else { "unread messages" };
        return Ok(format!("Inbox for '{me}': no {kind}."));
    }
    let mut out = format!("Inbox for '{me}' — {} message(s):", rows.len());
    for m in &rows {
        let bcast = if model::is_broadcast(&m.recipient) {
            " (broadcast)"
        } else {
            ""
        };
        let subj = m
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n\n#{} [{}] from {}{}{}\n{}",
            m.id,
            fmt_ts(m.ts),
            m.sender,
            bcast,
            subj,
            m.body
        ));
    }
    let mut footer = Vec::new();
    if mark_read {
        footer.push("marked read".to_string());
    }
    if remaining > 0 {
        footer.push(format!("{remaining} more unread"));
    }
    if !footer.is_empty() {
        out.push_str(&format!("\n\n({})", footer.join("; ")));
    }
    Ok(out)
}

fn tool_history(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let peer = args
        .get("peer")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let rows = store.history(&me, peer, limit).map_err(e)?;
    if rows.is_empty() {
        return Ok(match peer {
            Some(p) => format!("No history for '{me}' with '{p}'."),
            None => format!("No history for '{me}'."),
        });
    }
    let label = match peer {
        Some(p) => format!("'{me}' <-> '{p}'"),
        None => format!("involving '{me}' (incl. broadcasts)"),
    };
    let mut out = format!("History ({label}) — {} message(s):", rows.len());
    for m in &rows {
        let subj = m
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n\n#{} [{}] {} -> {}{}\n{}",
            m.id,
            fmt_ts(m.ts),
            m.sender,
            m.recipient,
            subj,
            m.body
        ));
    }
    Ok(out)
}

fn tool_sessions(store: &dyn Store, def: &Option<String>, _args: &Value) -> Result<String, String> {
    let me = def.clone().unwrap_or_default();
    let info = store.sessions().map_err(e)?;
    let total = store.total_messages().map_err(e)?;
    if info.is_empty() {
        return Ok("No sessions seen yet — the store is empty.".into());
    }
    let mut out = format!(
        "Known sessions ({}), {total} message(s) total:",
        info.len()
    );
    for (n, unread, last) in info {
        let mine = if !me.is_empty() && n == me {
            "  <- you"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n  • {n}: {unread} unread (last activity {}){mine}",
            fmt_ts(last)
        ));
    }
    Ok(out)
}

fn tool_clear(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("inbox");
    if scope == "all" {
        if !args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(
                "scope='all' wipes ALL messages for EVERY session irreversibly. \
                 Re-call with \"confirm\": true."
                    .into(),
            );
        }
        let n = store.clear_all().map_err(e)?;
        return Ok(format!("Wiped the store ({n} message(s) deleted)."));
    }
    let me = ident(args, "me", def)?;
    let n = store.clear_inbox(&me).map_err(e)?;
    Ok(format!("Marked {n} message(s) read for '{me}'."))
}

fn tool_peers(store: &dyn Store) -> Result<String, String> {
    let peers = store.list_peers().map_err(e)?;
    if peers.is_empty() {
        return Ok("No peers registered yet. Sessions register via `weave hook session`.".into());
    }
    let mut out = format!("Registered peers ({}):", peers.len());
    for p in peers {
        let inj = if inject::Target::from_peer(&p).injectable() {
            "injectable"
        } else {
            "no-inject"
        };
        let presence = if is_online(p.last_seen) {
            "online"
        } else {
            "offline"
        };
        out.push_str(&format!(
            "\n  • {} [{presence}] [{}] {} ({inj}) seen {}",
            p.name,
            p.mux,
            if p.target.is_empty() { "-" } else { &p.target },
            fmt_ts(p.last_seen)
        ));
    }
    Ok(out)
}

// ---- helpers ------------------------------------------------------------

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

fn reply(id: &Value, result: Value) -> String {
    json!({"jsonrpc":"2.0","id": id.clone(),"result": result}).to_string()
}

fn reply_err(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc":"2.0","id": id.clone(),"error":{"code":code,"message":message}}).to_string()
}

fn tools() -> Value {
    json!([
        {
            "name": "weave_send",
            "description": "Send a message to another agent session. 'to' = a session name, or 'all'/'*' to broadcast. If the recipient is a registered injectable peer (tmux/zellij), a live nudge is pushed into its pane immediately; otherwise it arrives on the recipient's next turn.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name, or 'all'."},
                "subject":{"type":"string"},
                "body":{"type":"string"}
            },"required":["to","body"]}
        },
        {
            "name": "weave_inbox",
            "description": "Read messages addressed to you. Unread-only + mark-read by default; with include_read=true it does not mark read unless mark_read=true.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string"},
                "include_read":{"type":"boolean"},
                "mark_read":{"type":"boolean"},
                "limit":{"type":"integer"}
            },"required":[]}
        },
        {
            "name": "weave_history",
            "description": "Read-only conversation view (never marks read). Optional 'peer' scopes to one session.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string"},"peer":{"type":"string"},"limit":{"type":"integer"}
            },"required":[]}
        },
        {
            "name": "weave_sessions",
            "description": "List session names seen, with unread counts and last activity.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_clear",
            "description": "scope='inbox' (default) marks your inbox read; scope='all' wipes the store (requires confirm=true).",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string"},"scope":{"type":"string","enum":["inbox","all"]},"confirm":{"type":"boolean"}
            },"required":[]}
        },
        {
            "name": "weave_peers",
            "description": "List registered peers and whether each is injectable (live push) or delivery-on-next-turn.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }
    ])
}
