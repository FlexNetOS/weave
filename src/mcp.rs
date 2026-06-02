//! MCP stdio server: newline-delimited JSON-RPC 2.0 on stdin/stdout. Exposes
//! weave's messaging tools. On send, if the recipient is a registered injectable
//! peer, a live nudge is pushed into their pane via the native injector.
//!
//! stdout is reserved for protocol messages; all logging goes to stderr.

use crate::inject::{self, Target};
use crate::model::{self, fmt_ts};
use crate::store::{clamp_limit, is_online, Store, MAX_LIMIT};
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
        // A per-line read error (e.g. invalid UTF-8 on the wire) must not be
        // fatal to the whole server. Log and skip it; one bad line cannot crash
        // the loop.
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log(&format!("stdin read error (skipping line): {e}"));
                continue;
            }
        };
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
            // A write/flush failure to a single client read must not tear down
            // the server. BrokenPipe means the client closed its read end → stop
            // cleanly; any other io error is logged and we keep serving.
            if let Err(e) = writeln!(stdout, "{resp}").and_then(|()| stdout.flush()) {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    log("stdout closed (broken pipe); exiting");
                    return Ok(());
                }
                log(&format!("stdout write error (continuing): {e}"));
            }
        }
    }
    log("stdin closed; exiting");
    Ok(())
}

/// Maximum accepted length (in characters) for a session identity — sender or
/// recipient. Identities flow into pane targets / nudge text, so an unbounded
/// value is both a footgun and a memory/log-spam vector. Generous enough for any
/// real session name yet tight enough to reject pasted garbage.
const MAX_IDENT_LEN: usize = 128;

/// Maximum accepted length (in characters) for a subject line. Subjects are
/// single-line metadata, not the payload (that's `body`), so they stay short.
const MAX_SUBJECT_LEN: usize = 256;

/// Validate and bound an identity string (sender/recipient). Rejects empty /
/// whitespace-only values and anything over [`MAX_IDENT_LEN`] characters, with a
/// clear, actionable error. Returns the trimmed identity on success.
///
/// `label` names the field for the error message (e.g. "from", "to", "sender").
fn bound_ident(label: &str, raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(format!("'{label}' must not be empty."));
    }
    // Count Unicode scalar values, not bytes, so multi-byte names aren't
    // penalised relative to ASCII ones.
    let n = t.chars().count();
    if n > MAX_IDENT_LEN {
        return Err(format!(
            "'{label}' is too long ({n} chars; max {MAX_IDENT_LEN}). Use a short session name."
        ));
    }
    Ok(t.to_string())
}

/// Validate and bound an optional subject line. `None`/blank yields `Ok(None)`;
/// an over-length subject is rejected with a clear error.
fn bound_subject(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => {
            let n = s.chars().count();
            if n > MAX_SUBJECT_LEN {
                return Err(format!(
                    "'subject' is too long ({n} chars; max {MAX_SUBJECT_LEN})."
                ));
            }
            Ok(Some(s.to_string()))
        }
    }
}

/// Resolve an identity from `args[key]`, falling back to the server default
/// (`$WEAVE_SESSION`). The resolved value is bounded via [`bound_ident`], so the
/// default is validated too. Empty + over-length both produce a clear error.
fn ident(args: &Value, key: &str, def: &Option<String>) -> Result<String, String> {
    if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            return bound_ident(key, s);
        }
    }
    if let Some(d) = def {
        if !d.trim().is_empty() {
            return bound_ident(key, d);
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
        _ => Some(reply_err(
            &id,
            -32601,
            &format!("Method not found: {method}"),
        )),
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
        "weave_reply" => tool_reply(store, me_default, args),
        "weave_thread" => tool_thread(store, args),
        "weave_receipts" => tool_receipts(store, args),
        "weave_doctor" => tool_doctor(store),
        "weave_whoami" => tool_whoami(store, me_default),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn tool_send(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    // `to` is bounded just like `from`: reject empty/whitespace and cap length.
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (session name, or 'all' to broadcast).")?;
    let to = bound_ident("to", to_raw)?;
    let to = to.as_str();
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let subject = subject.as_deref();

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
                let nudge =
                    format!("[weave] message from {from}: {body} (run weave_inbox to read)");
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

    let (rows, remaining) = store
        .inbox(&me, include_read, mark_read, limit)
        .map_err(e)?;
    if rows.is_empty() {
        let kind = if include_read {
            "messages"
        } else {
            "unread messages"
        };
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
    let mut out = format!("Known sessions ({}), {total} message(s) total:", info.len());
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
        if !args
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
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

/// Reply to an existing message. The recipient is derived by the store from the
/// parent message (it addresses the reply back to the parent's other party), so
/// the caller supplies only their identity, the parent id, and the body.
fn tool_reply(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let in_reply_to = args
        .get("in_reply_to")
        .and_then(|v| v.as_i64())
        .ok_or("'in_reply_to' is required (the message id you're replying to).")?;
    if in_reply_to <= 0 {
        return Err("'in_reply_to' must be a positive message id.".into());
    }
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;

    let mid = store.reply(&from, in_reply_to, body).map_err(e)?;
    let mut out = format!("Replied to #{in_reply_to} as message #{mid} from '{from}'.");

    // Native push: nudge the reply's recipient if it resolved to a registered
    // injectable peer. We look the recipient up from the freshly-stored reply so
    // the address matches whatever the store derived from the parent.
    if let Ok(rows) = store.thread(in_reply_to, clamp_limit(MAX_LIMIT)) {
        if let Some(reply_msg) = rows.iter().find(|m| m.id == mid) {
            let to = &reply_msg.recipient;
            if !model::is_broadcast(to) {
                if let Ok(Some(peer)) = store.get_peer(to) {
                    let target = Target::from_peer(&peer);
                    if target.injectable() {
                        let nudge =
                            format!("[weave] reply from {from}: {body} (run weave_inbox to read)");
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
        }
    }
    Ok(out)
}

/// Show a conversation thread rooted at `root_id` (the root and every reply that
/// descends from it), oldest-first as the store returns them. Read-only.
fn tool_thread(store: &dyn Store, args: &Value) -> Result<String, String> {
    let root_id = args
        .get("root_id")
        .and_then(|v| v.as_i64())
        .ok_or("'root_id' is required (the message id at the root of the thread).")?;
    if root_id <= 0 {
        return Err("'root_id' must be a positive message id.".into());
    }
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let rows = store.thread(root_id, limit).map_err(e)?;
    if rows.is_empty() {
        return Ok(format!("No thread found for root #{root_id}."));
    }
    let mut out = format!("Thread #{root_id} — {} message(s):", rows.len());
    for m in &rows {
        let subj = m
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        // Surface the reply linkage when present so the tree structure is legible.
        let reply_to = m
            .in_reply_to
            .map(|p| format!(" (reply to #{p})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n\n#{} [{}] {} -> {}{}{}\n{}",
            m.id,
            fmt_ts(m.ts),
            m.sender,
            m.recipient,
            subj,
            reply_to,
            m.body
        ));
    }
    Ok(out)
}

/// Show read receipts for a single message: who read it and when.
fn tool_receipts(store: &dyn Store, args: &Value) -> Result<String, String> {
    let message_id = args
        .get("message_id")
        .and_then(|v| v.as_i64())
        .ok_or("'message_id' is required (the message id to look up receipts for).")?;
    if message_id <= 0 {
        return Err("'message_id' must be a positive message id.".into());
    }
    let rows = store.receipts(message_id).map_err(e)?;
    if rows.is_empty() {
        return Ok(format!("No read receipts for #{message_id} yet."));
    }
    let mut out = format!("Receipts for #{message_id} — {} reader(s):", rows.len());
    for (reader, ts) in &rows {
        out.push_str(&format!("\n  • {reader} read at {}", fmt_ts(*ts)));
    }
    Ok(out)
}

/// Mirror the CLI `weave doctor` diagnostics over MCP.
///
/// Note: `db_path` and `config_path` are owned by `Config`, which is not plumbed
/// into the MCP server (it only receives the live `Store`). We surface every
/// diagnostic reachable from the store + current process environment; for the
/// db/config file locations, run the `weave doctor` CLI.
fn tool_doctor(store: &dyn Store) -> Result<String, String> {
    let target = inject::detect_target();
    let peers = store.list_peers().map_err(e)?;
    let online = peers.iter().filter(|p| is_online(p.last_seen)).count();
    let total = store.total_messages().map_err(e)?;
    let claude = inject::have("claude");
    let tgt = if target.id.is_empty() {
        "-"
    } else {
        &target.id
    };
    let mut out = String::from("weave doctor (mcp)");
    out.push_str(&format!(
        "\n  version:        {}",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!("\n  backend:        {}", store.backend()));
    out.push_str(&format!(
        "\n  this session:   mux={} target={} injectable={}",
        target.mux.as_str(),
        tgt,
        target.injectable()
    ));
    out.push_str(&format!("\n  messages:       {total}"));
    out.push_str(&format!(
        "\n  peers:          {} ({online} online)",
        peers.len()
    ));
    out.push_str(&format!(
        "\n  claude on PATH: {}",
        if claude { "yes" } else { "no" }
    ));
    out.push_str("\n  (db/config paths: run `weave doctor` on the CLI)");
    Ok(out)
}

/// Echo the resolved identity (default session) and the active storage backend,
/// plus how the current process would inject. Lets a caller confirm "who am I"
/// before sending.
fn tool_whoami(store: &dyn Store, def: &Option<String>) -> Result<String, String> {
    let identity = match def {
        Some(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => "(unset — pass 'from'/'me' explicitly)".to_string(),
    };
    let target = inject::detect_target();
    let tgt = if target.id.is_empty() {
        "-"
    } else {
        &target.id
    };
    Ok(format!(
        "identity:   {identity}\nbackend:    {}\nthis pane:  mux={} target={} injectable={}",
        store.backend(),
        target.mux.as_str(),
        tgt,
        target.injectable()
    ))
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
        },
        {
            "name": "weave_reply",
            "description": "Reply to a message by id. The recipient is derived from the parent message (the reply is addressed back to the parent's other party), so you only give your name, the parent id, and the body. Like weave_send, a live nudge is pushed if the recipient is an injectable peer.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "in_reply_to":{"type":"integer","description":"The message id you're replying to."},
                "body":{"type":"string"}
            },"required":["in_reply_to","body"]}
        },
        {
            "name": "weave_thread",
            "description": "Read-only: show a conversation thread (the root message and all replies descending from it) given the root message id.",
            "inputSchema": {"type":"object","properties":{
                "root_id":{"type":"integer","description":"The message id at the root of the thread."},
                "limit":{"type":"integer"}
            },"required":["root_id"]}
        },
        {
            "name": "weave_receipts",
            "description": "Show read receipts for a single message: which sessions have read it and when.",
            "inputSchema": {"type":"object","properties":{
                "message_id":{"type":"integer","description":"The message id to look up receipts for."}
            },"required":["message_id"]}
        },
        {
            "name": "weave_doctor",
            "description": "Diagnostics mirroring the `weave doctor` CLI: version, storage backend, this pane's injectability, total message count, and registered/online peers.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_whoami",
            "description": "Echo the resolved identity (default session from WEAVE_SESSION), the active storage backend, and how this process would inject. Use to confirm who you are before sending.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }
    ])
}
