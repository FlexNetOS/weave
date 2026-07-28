//! MCP stdio server: newline-delimited JSON-RPC 2.0 on stdin/stdout. Exposes
//! weave's messaging tools. On send, if the recipient is a registered injectable
//! peer, a live nudge is pushed into their pane via the native injector.
//!
//! stdout is reserved for protocol messages; all logging goes to stderr.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use weave_core::config::StoreSource;
use weave_core::memory;
use weave_core::model::{self, fmt_ts};
#[cfg(feature = "sign")]
use weave_core::sign;
use weave_core::store::{self, is_alive, Store};
use weave_inject::{Capability, Injector, Nudge, Target};

const SERVER_NAME: &str = "weave";
const SERVER_VERSION: &str = "0.1.0";
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn log(msg: &str) {
    eprintln!("[weave-mcp] {msg}");
}

/// PID file for the optional presence daemon.  Overridable via `WEAVE_PIDFILE`
/// so integration tests can use temp-scoped paths for parallel safety.
fn daemon_pidfile() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("WEAVE_PIDFILE") {
        return std::path::PathBuf::from(p);
    }
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| std::path::PathBuf::from(d).join("weave").join("weaved.pid"))
        .unwrap_or_else(|_| std::env::temp_dir().join("weaved.pid"))
}

/// argv-only probe: `kill -0 <pid>` returns success iff the process exists.
fn daemon_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

/// Run the server loop until stdin closes. `me_default` seeds the identity for
/// tools when the caller omits `me`/`from` (e.g. from $WEAVE_SESSION).
///
/// `nudge_template` is the user's configured live-injection nudge template
/// (`cfg.nudge_template`, plumbed from `main.rs` where the `Config` is in scope).
/// It mirrors the CLI's `Config::nudge` semantics: the `{from}` and `{body}`
/// placeholders are substituted, and — crucially — a template that omits `{body}`
/// is treated as a *quiet* preference, so the live push becomes a content-free
/// ping (via [`inject::inject_mode`] with [`Nudge::Nudge`]) instead of typing the
/// message body into the recipient's pane. `None` falls back to the built-in
/// default template (which embeds the body, i.e. a `Nudge::Full` push), preserving
/// today's behavior for callers that don't pass one.
/// Tier-2 pull + consent settings the MCP inbox drain needs, bundled so the long
/// `handle`/`call_tool`/`tool_inbox` chain threads ONE value instead of three.
/// Carries the validated `pull_from` sources plus the decision-5 consent state
/// (`inject_pulled` master toggle + the optional `allow_inject_from` finer gate)
/// so the drain can fire the caller-side consent nudge into THIS session's OWN
/// pane. This keeps the full `Config` out of `mcp` (the deliberate non-plumbing
/// noted below) while still letting the drain gate the nudge correctly.
pub struct PullConsent {
    /// Validated `pull_from` sources (the delivery allow-list): local store paths
    /// AND remote libSQL/Turso URLs (Tier-2 v2).
    pub from: Vec<StoreSource>,
    /// Decision-5 master toggle (default true): fire the consent nudge on a
    /// pulled message from an allow-listed source. `false` ⇒ pure queue-only.
    pub inject_pulled: bool,
    /// Optional finer gate; `None` ⇒ "same as `from`" (every pull source is
    /// inject-eligible). When `Some`, only listed sources trigger the nudge.
    pub allow_inject_from: Option<Vec<StoreSource>>,
    /// Tier-2 signed-identity verification policy (2d): the trust set, revocation
    /// list, and tri-state strict override. Forwarded to `pull_from_store`; inert
    /// without the `sign` feature. A revoked key's signed message and a forged
    /// signature are always rejected regardless.
    pub policy: store::VerifyPolicy,
}

impl PullConsent {
    /// An empty consent (no pull sources). For surfaces that only ever *write* via
    /// `dispatch_request` (e.g. the WL-052a dashboard write route) and never drain a
    /// cross-store inbox, so the pull-side gating is irrelevant.
    pub fn empty() -> Self {
        PullConsent {
            from: Vec::new(),
            inject_pulled: false,
            allow_inject_from: None,
            policy: store::VerifyPolicy::default(),
        }
    }

    /// Is `source` (an `allow`-listed pull source) permitted to trigger the consent
    /// nudge? Mirrors `Config::inject_allowed_from_source`: unset gate ⇒ every source
    /// is eligible; set gate ⇒ a Local matches by canonical path, a Remote matches by
    /// trailing-slash-normalized URL (never cross-kind).
    fn inject_allowed_from(&self, source: &StoreSource) -> bool {
        let allow = match &self.allow_inject_from {
            None => return true,
            Some(list) => list,
        };
        match source {
            StoreSource::Local(p) => {
                let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                allow.iter().any(|a| match a {
                    StoreSource::Local(ap) => {
                        std::fs::canonicalize(ap).unwrap_or_else(|_| ap.clone()) == key
                    }
                    StoreSource::Remote { .. } => false,
                })
            }
            StoreSource::Remote { url, .. } => {
                let key = url.strip_suffix('/').unwrap_or(url);
                allow.iter().any(|a| match a {
                    StoreSource::Remote { url: au, .. } => {
                        au.strip_suffix('/').unwrap_or(au) == key
                    }
                    StoreSource::Local(_) => false,
                })
            }
        }
    }
}

/// Sign a cross-store intent's canonical `(from,to,body)` with this session's
/// configured signing key (only with `--features sign`), returning the hex
/// signature for `outbox.sig`, or `""` when no key is configured / the feature is
/// off. A load error is non-fatal (logs to stderr, sends unsigned). The private key
/// is never logged. Mirror of `main::sign_intent_if_keyed`.
#[cfg(feature = "sign")]
fn sign_intent_if_keyed(from: &str, to: &str, body: &str) -> String {
    match sign::load_signing_key() {
        Ok(Some(key)) => sign::sign_intent(&key, from, to, body),
        Ok(None) => String::new(),
        Err(err) => {
            log(&format!(
                "could not load signing key (sending unsigned): {err}"
            ));
            String::new()
        }
    }
}

#[cfg(not(feature = "sign"))]
fn sign_intent_if_keyed(_from: &str, _to: &str, _body: &str) -> String {
    String::new()
}

pub fn serve<I: Injector>(
    store: &dyn Store,
    me_default: Option<String>,
    me_default_is_guess: bool,
    nudge_template: Option<&str>,
    extra_dbs: Vec<StoreSource>,
    pull: PullConsent,
    injector: &I,
) -> Result<()> {
    log(&format!(
        "starting; backend={} default_session={:?} guess={me_default_is_guess}",
        store.backend(),
        me_default
    ));
    let mut me_default = me_default;
    // WL-084: when the default identity is a basename(cwd) GUESS, it can name
    // another session's peer (same-basename collision → this session was
    // auto-uniquified at SessionStart) — or no row at all (the MCP server often
    // boots BEFORE the SessionStart hook registers). Re-pin lazily: before each
    // request, until a hit, look up the row owned by our own long-lived client
    // process and adopt ITS name. A miss keeps the guess (pre-WL-084 behavior).
    let mut repin_done = !me_default_is_guess;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        if !repin_done {
            if let Some(owned) = client_owned_peer_name(store) {
                if me_default.as_deref() != Some(owned.as_str()) {
                    log(&format!(
                        "WL-084 re-pin: default identity {:?} -> '{owned}' (row owned by this session's client process)",
                        me_default
                    ));
                }
                me_default = Some(owned);
                repin_done = true;
            }
        }
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
        if let Some(resp) = dispatch_request(
            store,
            &me_default,
            nudge_template,
            &extra_dbs,
            &pull,
            &req,
            injector as &dyn Injector,
            true,
        ) {
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

/// WL-084: the peer row registered by THIS session — matched by the long-lived
/// client process pid on this host (the same anchor the SessionStart hook
/// stores), so the MCP server and the hooks agree on identity even when the
/// alias was auto-uniquified. `None` on no/ambiguous match or any store error
/// (never guess here — the caller already holds the basename guess).
fn client_owned_peer_name(store: &dyn Store) -> Option<String> {
    let pid = store::client_pid()?;
    let host = weave_core::config::this_host();
    let peers = store.list_peers().ok()?;
    let mut owned = peers
        .into_iter()
        .filter(|p| p.pid == Some(pid) && p.host == host);
    let first = owned.next()?;
    if owned.next().is_some() {
        return None;
    }
    Some(first.name)
}

/// Maximum accepted length (in characters) for a session identity — sender or
/// recipient. Identities flow into pane targets / nudge text, so an unbounded
/// value is both a footgun and a memory/log-spam vector. Generous enough for any
/// real session name yet tight enough to reject pasted garbage.
const MAX_IDENT_LEN: usize = 128;

/// Maximum accepted length (in characters) for a subject line. Subjects are
/// single-line metadata, not the payload (that's `body`), so they stay short.
const MAX_SUBJECT_LEN: usize = 256;

/// WL-051 / ADR-0003: the **standing-token budget** for the MCP surface. The
/// serialized bytes of the default `tools/list` payload must stay under this ceiling
/// **regardless of how many operations exist** — `token-light` is a first-class
/// invariant, a peer of `dependency-light`: *adding a feature must not add standing
/// tokens*. ~8 KB ≈ the ADR's ≤~2k-token target (the progressive-disclosure meta-tool
/// is currently ~1.4 KB). Reverting to an eager-flat table, or piling on standing
/// dispatcher tools, trips the [`standing_mcp_surface_is_within_token_budget`] guard.
/// (The eager-flat compatibility mode, `WEAVE_MCP_EAGER=1`, is exempt — it is an
/// explicit opt-in, not the standing default.)
pub const MAX_STANDING_TOOLS_BYTES: usize = 8192;

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

#[allow(clippy::too_many_arguments)]
/// Dangerous/mutating tools that are disabled by default in HTTP transport mode.
const DANGEROUS_TOOLS: &[&str] = &[
    "weave_send",
    // WL-056 / ADR-0005: cross-machine PUSH receive handler. It WRITES B's inbox
    // (the Tier-2 commit pipeline, A-initiated), so it is a mutating op — gated in
    // safe HTTP mode exactly like `weave_send`. It is reachable only when the
    // operator runs `weave serve --write` (which dispatches with `dangerous=true`).
    "weave_push",
    "weave_notify",
    "weave_reply",
    "weave_ask",
    "weave_answer",
    "weave_ack",
    "weave_clear",
    "weave_schedule",
    "weave_schedules",
    "weave_tick",
    "weave_job_create",
    "weave_job_delegate",
    "weave_job_claim",
    "weave_job_update",
    "weave_job_cancel",
    "weave_claim_orchestrator",
    "weave_review_add",
    "weave_review_mark",
    "weave_review_remove",
    "weave_ask_permission",
    "weave_permission_resolve",
    "weave_memory_write",
    "weave_memory_delete",
    "weave_setup",
    "weave_uninstall",
    "weave_daemon_start",
    "weave_daemon_stop",
    "weave_set_message_priority",
    "weave_set_peer_policy",
    "weave_spawn_peer",
    "weave_kill_peer",
    // WL-049 / ADR-0002: stealth web access is powerful + abuse-prone → dangerous,
    // so it is blocked in safe HTTP mode (only available over the trusted stdio
    // transport or with --dangerous). It is additionally deny-by-default by policy.
    "weave_web",
];

/// True if `name` is a dangerous tool that should be filtered in safe mode.
pub fn is_dangerous_tool(name: &str) -> bool {
    DANGEROUS_TOOLS.contains(&name)
}

/// Dispatch a single JSON-RPC request and return the JSON response string.
/// Notifications (no id) return `None`. Used by both stdio and HTTP transports.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_request(
    store: &dyn Store,
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    req: &Value,
    injector: &dyn Injector,
    dangerous: bool,
) -> Option<String> {
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
            if !dangerous && is_dangerous_tool(name) {
                return Some(reply_err(
                    &id,
                    -32603,
                    &format!(
                        "Tool '{name}' is disabled in safe HTTP mode. Start with --dangerous to enable."
                    ),
                ));
            }
            match call_tool(
                store,
                me_default,
                nudge_template,
                extra_dbs,
                pull,
                name,
                &args,
                injector,
                dangerous,
            ) {
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

#[allow(clippy::too_many_arguments)]
fn call_tool(
    store: &dyn Store,
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    name: &str,
    args: &Value,
    injector: &dyn Injector,
    dangerous: bool,
) -> Result<String, String> {
    match name {
        // WL-050 / ADR-0003: token-light meta-tool — the full op set on demand.
        "weave" => tool_meta(
            store,
            me_default,
            nudge_template,
            extra_dbs,
            pull,
            args,
            injector,
            dangerous,
        ),
        "weave_send" => tool_send(store, me_default, nudge_template, args, injector),
        // WL-056 / ADR-0005: cross-machine PUSH receive handler (the A-initiated dual
        // of the Tier-2 pull-commit). Reached over the bearer-gated `POST /api` surface.
        "weave_push" => tool_push(store, me_default, pull, args, injector),
        "weave_notify" => tool_notify(store, me_default, nudge_template, args, injector),
        "weave_broadcast_notify" => {
            tool_broadcast_notify(store, me_default, nudge_template, args, injector)
        }
        "weave_broadcast_ask" => {
            tool_broadcast_ask(store, me_default, nudge_template, args, injector)
        }
        "weave_delivery" => tool_delivery(store, args),
        "weave_outbox" => tool_outbox(store, args),
        "weave_inbox" => tool_inbox(store, me_default, pull, args, injector),
        "weave_history" => tool_history(store, me_default, args),
        "weave_search" => tool_search(store, args),
        "weave_sessions" => tool_sessions(store, me_default, extra_dbs, args),
        "weave_clear" => tool_clear(store, me_default, args),
        "weave_peers" => tool_peers(store, me_default, extra_dbs, args, injector),
        "weave_scan" => tool_scan(store, me_default, extra_dbs, args, injector),
        "weave_reply" => tool_reply(store, me_default, nudge_template, args, injector),
        "weave_thread" => tool_thread(store, args),
        "weave_receipts" => tool_receipts(store, args),
        "weave_doctor" => tool_doctor(store, extra_dbs, injector),
        "weave_whoami" => tool_whoami(store, me_default, injector),
        "weave_attach" => tool_attach(store, me_default, args, injector),
        "weave_spawn_peer" => tool_spawn_peer(store, me_default, args, injector),
        "weave_kill_peer" => tool_kill_peer(store, args, injector),
        "weave_set_description" => tool_set_description(store, me_default, args),
        "weave_set_turn_state" => tool_set_turn_state(store, me_default, args),
        "weave_set_message_priority" => tool_set_message_priority(store, args),
        "weave_set_peer_policy" => tool_set_peer_policy(store, args),
        "weave_get_peer_policy" => tool_get_peer_policy(store, args),
        "weave_connect" => tool_connect(store, args, injector),
        "weave_ask" => tool_ask(store, me_default, nudge_template, args, injector),
        "weave_answer" => tool_answer(store, me_default, nudge_template, args, injector),
        "weave_ack" => tool_ack(store, me_default, args),
        "weave_asks" => tool_asks(store, me_default, args),
        "weave_ask_get" => tool_ask_get(store, args),
        "weave_ask_status" => tool_ask_status(store, args),
        "weave_responder" => tool_responder(store, me_default, nudge_template, args, injector),
        "weave_ask_many" => tool_ask_many(store, me_default, nudge_template, args, injector),
        "weave_ask_many_result" => tool_ask_many_result(store, args),
        "weave_job_create" => tool_job_create(store, me_default, args),
        "weave_job_delegate" => {
            tool_job_delegate(store, me_default, nudge_template, args, injector)
        }
        "weave_job_list" => tool_job_list(store, args),
        // `show` is the canonical detail view; `status` is its alias (repowire parity).
        "weave_job_show" | "weave_job_status" => tool_job_status(store, args),
        "weave_job_claim" => tool_job_claim(store, me_default, args),
        "weave_job_update" => tool_job_update(store, args),
        "weave_job_result" => tool_job_result(store, args),
        "weave_job_cancel" => tool_job_cancel(store, me_default, args),
        "weave_claim_orchestrator" => tool_claim_orchestrator(store, me_default, args),
        "weave_orchestrator_status" => tool_orchestrator_status(store, args),
        "weave_daemon_start" => tool_daemon_start(me_default, args),
        "weave_daemon_stop" => tool_daemon_stop(),
        "weave_daemon_status" => tool_daemon_status(),
        "weave_schedule" => tool_schedule(store, me_default, args),
        "weave_schedules" => tool_schedules(store, me_default, args),
        "weave_cancel_schedule" => tool_cancel_schedule(store, args),
        "weave_tick" => tool_tick(store, me_default, args),
        "weave_memory_write" => tool_memory_write(me_default, args),
        "weave_memory_read" => tool_memory_read(me_default, args),
        "weave_memory_search" => tool_memory_search(me_default, args),
        "weave_memory_list" => tool_memory_list(me_default, args),
        "weave_memory_delete" => tool_memory_delete(me_default, args),
        "weave_review_queue" => tool_review_queue(store, args),
        "weave_review_add" => tool_review_add(store, args),
        "weave_review_mark" => tool_review_mark(store, me_default, args),
        "weave_review_remove" => tool_review_remove(store, args),
        "weave_ask_permission" => {
            tool_ask_permission(store, me_default, nudge_template, args, injector)
        }
        "weave_permission_status" => tool_permission_status(store, args),
        "weave_permission_list" => tool_permission_list(store, me_default, args),
        "weave_lease_reserve" => tool_lease_reserve(store, me_default, args),
        "weave_lease_release" => tool_lease_release(store, me_default, args),
        "weave_lease_list" => tool_lease_list(store, args),
        "weave_lease_sweep" => tool_lease_sweep(store),
        "weave_thread_summarize" => tool_thread_summarize(store, args),
        "weave_summarize_text" => tool_summarize_text(args),
        "weave_web" => tool_web(store, me_default, args, injector),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

/// The built-in default nudge template, used when the user has not configured a
/// `nudge_template`. Kept in sync with `Config::nudge`'s default so MCP and CLI
/// pushes read identically out of the box. Placeholders: `{from}`, `{body}`.
const DEFAULT_NUDGE_TEMPLATE: &str =
    "[weave] message from {from}: {body} (run weave_inbox to read)";

/// Render the live-injection nudge for a message and decide its injection mode
/// from the user's configured template (mirrors `Config::nudge`):
///   * `{from}`/`{body}` placeholders are substituted into the returned line;
///   * if the template contains `{body}`, the rendered line carries the content
///     and we inject it verbatim ([`Nudge::Full`]);
///   * if the template OMITS `{body}`, the user has opted into a quiet preference,
///     so we return [`Nudge::Nudge`] — [`inject::inject_mode`] then types only its
///     fixed content-free ping, never the body, regardless of the rendered text.
///
/// `template` is the user's `cfg.nudge_template` (`None` ⇒ [`DEFAULT_NUDGE_TEMPLATE`],
/// which embeds `{body}` and therefore yields a `Full` push, preserving the
/// historical behavior).
fn build_nudge(template: Option<&str>, from: &str, body: &str) -> (String, Nudge) {
    let tmpl = template.unwrap_or(DEFAULT_NUDGE_TEMPLATE);
    let mode = if tmpl.contains("{body}") {
        Nudge::Full
    } else {
        Nudge::Nudge
    };
    let rendered = tmpl.replace("{from}", from).replace("{body}", body);
    (rendered, mode)
}

fn tool_send(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    // `to` is bounded just like `from`: reject empty/whitespace and cap length.
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (session name, or 'all' to broadcast).")?;
    let to_bound = bound_ident("to", to_raw)?;
    let to = if model::is_broadcast(&to_bound) {
        to_bound
    } else {
        store::resolve_point_recipient(store, &to_bound).map_err(e)?
    };
    let to = to.as_str();
    let no_memory = args
        .get("no_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body_raw = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let body = maybe_prefix_body_mcp(&from, body_raw, no_memory);
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let subject = subject.as_deref();
    let idempotency_key = args
        .get("idempotencyKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    // WL-037: optional id of a prior message this one replaces. Validated as a
    // positive message id before any DB bind (the `in_reply_to` precedent).
    let supersedes = match args.get("supersedes").and_then(|v| v.as_i64()) {
        Some(id) if id <= 0 => {
            return Err("'supersedes' must be a positive message id.".into());
        }
        other => other,
    };
    // WL-038: optional ephemeral TTL (seconds). Validated against the cap at this
    // seam (the `lease_ttl_valid` precedent) before any DB bind.
    let ttl = match args.get("ttl").and_then(|v| v.as_i64()) {
        Some(t) if !model::ttl_valid(t) => {
            return Err(format!(
                "'ttl' must be between 1 and {} seconds.",
                model::MAX_MSG_TTL_SECS
            ));
        }
        other => other,
    };
    let trace_id = Some(model::mint_trace_id());

    // Cross-store routing (Tier-2): when `to_store` is supplied, the recipient
    // lives in a FOREIGN store, so deposit an intent into OUR OWN outbox rather
    // than attempt any foreign write (owner-only-writes). No local inbox row, no
    // inject — we cannot reach the recipient's pane across stores; the receiver
    // pulls and commits it on its next drain.
    if let Some(store_path) = args
        .get("to_store")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        if model::is_broadcast(to) {
            return Err(
                "cross-store broadcast is not supported; send to a named recipient \
                 (Tier-2 is directed-only)."
                    .to_string(),
            );
        }
        let to_host = args
            .get("to_host")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        // Signed identity (2d): sign the canonical (from,to,body) with this
        // session's key when one is configured (and the `sign` feature is built),
        // so the receiver can verify `from` is unforgeable. "" otherwise (advisory).
        let sig = sign_intent_if_keyed(&from, to, &body);
        let id = store
            .enqueue_intent(
                to,
                to_host,
                &from,
                subject,
                &body,
                &sig,
                idempotency_key,
                trace_id.as_deref(),
                priority,
                ttl.unwrap_or(0),
            )
            .map_err(e)?;
        // WL-036: a queued cross-store intent IS a send ⇒ fire `Send` hooks. The
        // message was NOT delivered locally (no inbox row / inject), but the send
        // event happened. Best-effort; never sinks the queued result.
        weave_inject::fire_post_send_hooks(
            &weave_core::config::Config::load(),
            weave_core::config::HookEvent::Send,
            &from,
            to,
            subject.unwrap_or(""),
            id,
        );
        return Ok(format!(
            "Queued intent #{id} from '{from}' for '{to}' @ {store_path} (delivered on their next drain)."
        ));
    }

    let mid = store
        .send(
            &from,
            to,
            subject,
            &body,
            idempotency_key,
            trace_id.as_deref(),
        )
        .map_err(e)?;
    if let Some(p) = priority {
        let _ = store.set_message_priority(mid, p);
    }
    // WL-038: post-stamp the ephemeral expiry after the send (the
    // `set_message_priority` post-stamp precedent). `ttl` is already cap-validated.
    if let Some(t) = ttl {
        let _ = store.set_message_expiry(mid, model::expiry_from_ttl(model::now(), t));
    }
    // WL-037: post-stamp the supersede link after the send (the `set_message_priority`
    // post-stamp precedent). Authorization (caller == original sender) + id existence
    // are enforced in `Store::supersede`; a bad id surfaces as an error string.
    if let Some(old) = supersedes {
        store.supersede(&from, old, mid).map_err(e)?;
    }
    let dest = if model::is_broadcast(to) {
        "broadcast"
    } else {
        to
    };
    let mut out = if let Some(old) = supersedes {
        format!("Sent message #{mid} from '{from}' to '{dest}' (supersedes #{old}).")
    } else {
        format!("Sent message #{mid} from '{from}' to '{dest}'.")
    };

    // P6 delivery trace (point-to-point only — broadcast is not injected and is not
    // traced in P6, keeping the trace bounded). Best-effort: never sinks the send.
    if !model::is_broadcast(to) {
        record_delivery_best_effort(
            store,
            mid,
            model::DeliveryRefKind::Message,
            to,
            model::DeliveryStage::Queued,
            model::DeliveryOutcome::Ok,
        );
        // Native push: nudge the recipient's pane if it's a registered injectable peer.
        if let Ok(Some(peer)) = store.get_peer(to) {
            let ambiguous = mcp_peer_ambiguous_target_names(store, to);
            let target = Target::from_peer(&peer);
            // Record the post-inject stage AFTER the inject attempt (no store→inject
            // edge — the store records the outcome we pass it). Ambiguous shared
            // mux targets are deliberately not injected: the message remains queued
            // and the trace records the safety downgrade.
            let (stage, outcome) = if !ambiguous.is_empty() {
                out.push_str(&format!(
                    " Live injection avoided: ambiguous mux target shared by {} (ambiguous_target_queued).",
                    ambiguous.join(", ")
                ));
                (
                    model::DeliveryStage::NotInjectable,
                    model::DeliveryOutcome::AmbiguousTarget,
                )
            } else if target.injectable() {
                let (nudge, mode) = build_nudge(nudge_template, &from, &body);
                match injector.inject_mode(&target, &nudge, mode) {
                    Ok(true) => {
                        out.push_str(&format!(
                            " Injected live nudge into {} target '{}'.",
                            target.mux.as_str(),
                            target.id
                        ));
                        (model::DeliveryStage::Injected, model::DeliveryOutcome::Ok)
                    }
                    Ok(false) => (model::DeliveryStage::Queued, model::DeliveryOutcome::Ok),
                    Err(err) => {
                        out.push_str(&format!(
                            " (peer registered on {} but inject failed: {err}; it'll arrive on their next turn)",
                            target.mux.as_str()
                        ));
                        (
                            model::DeliveryStage::InjectFailed,
                            model::DeliveryOutcome::Fail,
                        )
                    }
                }
            } else {
                (
                    model::DeliveryStage::NotInjectable,
                    model::DeliveryOutcome::Ok,
                )
            };
            record_delivery_best_effort(
                store,
                mid,
                model::DeliveryRefKind::Message,
                to,
                stage,
                outcome,
            );
        } else {
            // No peer row ⇒ not injectable; record so the trace is complete.
            record_delivery_best_effort(
                store,
                mid,
                model::DeliveryRefKind::Message,
                to,
                model::DeliveryStage::NotInjectable,
                model::DeliveryOutcome::Ok,
            );
        }
    }
    // WL-036: best-effort post-send hooks, fired AFTER persist + inject. Failures log
    // to STDERR (never the JSON-RPC stdout frame) and never sink the send. Config is
    // loaded here (the `Config::load()` precedent already used by other MCP tools) so
    // the full `Config` need not be plumbed through `serve`.
    weave_inject::fire_post_send_hooks(
        &weave_core::config::Config::load(),
        weave_core::config::HookEvent::Send,
        &from,
        to,
        subject.unwrap_or(""),
        mid,
    );
    Ok(out)
}

/// WL-056 / ADR-0005: cross-machine PUSH **receive** handler — the A-initiated DUAL
/// of the Tier-2 pull-commit, reached over the bearer-gated `POST /api` surface
/// (`--features surfaces` + `weave serve --write`). A sender A on machine 1 POSTs a
/// signed `Intent` here; B (this handler, on machine 2) commits it into **B's OWN**
/// inbox and lights B's OWN pane — **without B polling**.
///
/// This REUSES the pull machinery verbatim — it does NOT reinvent verification or
/// commit:
///   1. Parse the args into a `model::Intent` (the push wire form).
///   2. Build the receiver `VerifyPolicy` from `Config` exactly as the pull path
///      (`main::verify_policy`) does — trust set, revocation list, strict override.
///   3. Commit via the EXISTING `store::commit_pulled(store, me, "push:<from>",
///      &policy, vec![intent])` — re-validation, signature verification
///      (`verify_pulled_intent` → `sign::verify_intent`), `Store::send` (B assigns
///      id/ts), and `Intent.idempotency_key` dedup are all inherited unchanged.
///   4. On `committed == 1`, fire the EXISTING caller-side `nudge_pulled(...)` into
///      B's own pane (gated by the same `inject_pulled` + `inject_allowed_from`
///      consent as a pull).
///
/// Owner-only-writes: A never writes B's store — B's own handler does every write.
/// Idempotency: push has no per-source `pull_cursor`, so dedup rests entirely on the
/// Intent's `idempotency_key`; the SEND path always populates it (synthesizing one
/// when A omits it), so a retried POST never double-commits.
fn tool_push(
    store: &dyn Store,
    _def: &Option<String>,
    pull: &PullConsent,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    // `from`/`to`/`body` are required; the rest are optional (serde defaults).
    let from = args
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or("'from' is required (the sender's session name).")?;
    let from = bound_ident("from", from)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the recipient session name on THIS machine).")?;
    let to = bound_ident("to", to_raw)?;
    if model::is_broadcast(&to) {
        return Err("cross-machine push is directed-only; push to a named recipient.".to_string());
    }
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?
        .to_string();
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let to_host = args
        .get("to_host")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let sig = args
        .get("sig")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(model::default_priority);
    let ttl = match args.get("ttl").and_then(|v| v.as_i64()) {
        Some(t) if !model::ttl_valid(t) => {
            return Err(format!(
                "'ttl' must be between 1 and {} seconds.",
                model::MAX_MSG_TTL_SECS
            ));
        }
        other => other.unwrap_or(0),
    };
    let trace_id = args
        .get("trace_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| Some(model::mint_trace_id()));
    // Idempotency: push has no `pull_cursor`, so dedup rests on the key. The send
    // path ALWAYS populates it, but defend in depth here too — synthesize a stable
    // key from `(from, body)` if a keyless push somehow arrives, so a retried POST
    // never double-commits.
    let idempotency_key = args
        .get("idempotency_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| synth_push_idempotency_key(&from, &body));

    // The push wire form is an Intent. `id`/`ts` are advisory on the wire — B
    // re-stamps them on commit via `Store::send` (id/ts B-local).
    let intent = model::Intent {
        id: 0,
        ts: model::now(),
        to: to.clone(),
        to_host,
        from: from.clone(),
        subject,
        body,
        sig,
        idempotency_key: Some(idempotency_key),
        trace_id,
        priority,
        ttl,
    };

    // Build the receiver VerifyPolicy from Config exactly as the pull path does
    // (`main::verify_policy`). The dashboard `POST /api` route hands us
    // `PullConsent::empty()` (advisory), so we do NOT rely on `pull.policy` for the
    // verification decision — we load it fresh, like every other Config-driven MCP
    // tool (the `Config::load()` precedent). This keeps verify-on-commit honest even
    // when the surface itself carries no pull config.
    let cfg = weave_core::config::Config::load();
    let policy = store::VerifyPolicy {
        strict_override: cfg.strict_verify_override(),
        trust: cfg.trust_set(),
        revoked: cfg.revoked_set(),
    };

    // Commit via the EXISTING pull-commit pipeline (no new Store method, no schema
    // change). `commit_pulled` re-validates, verifies the signature under `policy`,
    // commits into B's OWN inbox via `Store::send` (B assigns id/ts), and dedups on
    // the idempotency key. A forged/unsigned-from-trusted intent is rejected (0
    // committed) before any write.
    let source = format!("push:{from}");
    let committed = store::commit_pulled(store, &to, &source, &policy, vec![intent]).map_err(e)?;

    if committed == 0 {
        return Err(format!(
            "push from '{from}' to '{to}' rejected at commit (verification/validation failed) \
             or already delivered (idempotent)."
        ));
    }

    // Caller-side consent nudge into B's OWN pane (mirrors the EXISTING `nudge_pulled`
    // seam) — so a push lights B's pane exactly as a pull does, WITHOUT B polling.
    // Consent gating mirrors a pull: the `inject_pulled` master toggle must be on (the
    // surface's wired value if any, else the freshly-loaded Config), AND no finer
    // `allow_inject_from` gate may be set — the push source is synthetic (`push:<from>`)
    // and so cannot appear in an allow-list, so a configured finer gate keeps a push
    // delivery advisory (queue-only), exactly as it would for a non-allow-listed pull.
    // Best-effort: a nudge failure never sinks the (already committed) push.
    let inject_pulled = pull.inject_pulled || cfg.inject_pulled();
    let finer_gate = if pull.inject_pulled {
        pull.allow_inject_from.is_some()
    } else {
        cfg.allow_inject_from_sources().is_some()
    };
    if inject_pulled && !finer_gate {
        nudge_pulled_push(store, &to, injector);
    }

    Ok(format!(
        "Pushed intent from '{from}' delivered to '{to}' (#committed=1, id assigned locally)."
    ))
}

/// Synthesize a stable idempotency key for a keyless push: `push:<from>:<ts>:<hash>`
/// where `<hash>` is an FNV-1a digest of the body (no `rand`/hash crate — weave is
/// dependency-light). Two identical-body pushes from the same sender in the same
/// second collapse to one row; the send path normally supplies an explicit key, so
/// this is a defense-in-depth fallback for a keyless retried POST.
fn synth_push_idempotency_key(from: &str, body: &str) -> String {
    // FNV-1a 64-bit over the body bytes.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in body.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("push:{from}:{:x}", h)
}

/// Caller-side consent nudge for a committed push (mirrors `nudge_pulled` but for the
/// synthetic push source). After a push commits into B's own inbox, fire the EXISTING
/// paste-safe content-free [`Nudge::Nudge`] into B's OWN registered pane — never a
/// foreign pane, never the body. Best-effort: any failure is logged to STDERR (never
/// stdout) and never breaks the push.
fn nudge_pulled_push(store: &dyn Store, me: &str, injector: &dyn Injector) {
    let peer = match store.get_peer(me) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(err) => {
            log(&format!("push-nudge skipped (non-fatal): {err}"));
            return;
        }
    };
    let target = Target::from_peer(&peer);
    if !target.injectable() || !injector.target_alive(&target) {
        return;
    }
    match injector.inject_mode(&target, "", Nudge::Nudge) {
        Ok(_) => {}
        Err(err) => log(&format!("push-nudge inject failed (non-fatal): {err}")),
    }
}

/// Best-effort delivery-trace write: append one metadata-only stage row, swallowing
/// (and logging to STDERR) any store error so a trace failure can NEVER sink or slow
/// the delivery hot path. Mirrors the `set_turn_state_best_effort` / gc precedent. The
/// store records the OUTCOME passed here AFTER the inject already happened — there is
/// NO `store → inject` edge.
fn record_delivery_best_effort(
    store: &dyn Store,
    ref_id: i64,
    kind: model::DeliveryRefKind,
    to: &str,
    stage: model::DeliveryStage,
    outcome: model::DeliveryOutcome,
) {
    if let Err(err) =
        store.record_delivery(ref_id, kind.as_str(), to, stage.as_str(), outcome.as_str())
    {
        // STDOUT DISCIPLINE: trace diagnostics go to stderr, never the JSON-RPC frame.
        log(&format!("delivery-trace write failed (non-fatal): {err}"));
    }
}

/// Map a normalized P1 verdict token to the post-inject trace (stage, outcome). The
/// pure verdict→stage fold, unit-tested for exhaustiveness:
/// `transport_delivered` ⇒ `Injected/Ok`; `recipient_not_injectable` ⇒
/// `NotInjectable/Ok`; anything else (`queued_next_turn`) ⇒ `Queued/Ok`.
///
/// All are `Ok`: a queued/not-injectable delivery is success (the message is safely
/// in the store). A precise `InjectFailed/Fail` is recorded separately at the
/// inline-inject site (notify/send) where the raw `Err` is visible.
fn verdict_to_stage(verdict: &str) -> (model::DeliveryStage, model::DeliveryOutcome) {
    match verdict {
        "transport_delivered" => (model::DeliveryStage::Injected, model::DeliveryOutcome::Ok),
        "recipient_not_injectable" => (
            model::DeliveryStage::NotInjectable,
            model::DeliveryOutcome::Ok,
        ),
        "ambiguous_target_queued" => (
            model::DeliveryStage::NotInjectable,
            model::DeliveryOutcome::AmbiguousTarget,
        ),
        _ => (model::DeliveryStage::Queued, model::DeliveryOutcome::Ok),
    }
}

/// `weave_notify`: fire-and-forget point-to-point notification (NO reply expected).
/// THIN over the existing send + P1 verdict seam — it does NOT fork `tool_send`:
/// it persists a normal stored message via `store.send`, fires the SAME caller-side
/// live nudge, and RETURNS the normalized honest verdict token
/// (transport_delivered / queued_next_turn / recipient_not_injectable). What it adds
/// over `weave_send` is the explicit no-reply intent + the normalized verdict token;
/// what it adds over `weave_ask` is that it opens NO tracked thread. Point-to-point
/// only — broadcast notify is deferred (use `weave_send` for broadcast).
fn tool_notify(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the peer session name to notify).")?;
    let to = bound_ident("to", to_raw)?;
    // Point-to-point only: reject broadcast with a pointer to send (mirrors tool_ask).
    if model::is_broadcast(&to) {
        return Err(
            "notify is point-to-point (no reply expected); use weave_send for broadcast \
             (broadcast notify is deferred)."
                .to_string(),
        );
    }
    let to = store::resolve_point_recipient(store, &to).map_err(e)?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let idempotency_key = args
        .get("idempotencyKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    // WL-038: optional ephemeral TTL, cap-validated at the seam.
    let ttl = match args.get("ttl").and_then(|v| v.as_i64()) {
        Some(t) if !model::ttl_valid(t) => {
            return Err(format!(
                "'ttl' must be between 1 and {} seconds.",
                model::MAX_MSG_TTL_SECS
            ));
        }
        other => other,
    };
    let trace_id = Some(model::mint_trace_id());

    // Persist via the EXISTING send path (no new persistence — notify is a normal
    // stored message; "no reply" is a caller-intent label, not a schema distinction).
    // `store.send` enforces MAX_BODY via check_body, so an oversized body is a clean
    // error (never a panic / partial persist).
    let mid = store
        .send(
            &from,
            &to,
            subject.as_deref(),
            body,
            idempotency_key,
            trace_id.as_deref(),
        )
        .map_err(e)?;
    if let Some(p) = priority {
        let _ = store.set_message_priority(mid, p);
    }
    // WL-038: post-stamp the ephemeral expiry after persist.
    if let Some(t) = ttl {
        let _ = store.set_message_expiry(mid, model::expiry_from_ttl(model::now(), t));
    }
    // WL-039: opt-in idle-notification dedup. Stamp this ping idle and supersede
    // this sender's prior UNREAD idle pings to `to` (collapse "still waiting"
    // pings to the latest). Best-effort post-persist — a dedup failure never sinks
    // the notify. Never touches a real message or another sender's pings.
    let dedup_idle = args
        .get("dedupIdle")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if dedup_idle {
        let _ = store.supersede_prior_idle(&from, &to, mid);
    }

    // Trace: queued after persist (best-effort, never sinks the path).
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Notify,
        &to,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );

    // Caller-side live nudge + honest verdict (REUSE the P1 helper — no store→inject
    // edge). The helper folds the raw inject Err into `queued_next_turn`; that is the
    // verdict we surface. The trace records the matching post-inject stage.
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &to, body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Notify,
        &to,
        stage,
        outcome,
    );

    // WL-036: notify is a point-to-point send ⇒ fire `Send` hooks (best-effort).
    weave_inject::fire_post_send_hooks(
        &weave_core::config::Config::load(),
        weave_core::config::HookEvent::Send,
        &from,
        &to,
        subject.as_deref().unwrap_or(""),
        mid,
    );

    Ok(format!(
        "Notified '{to}' (#{mid}, no reply expected). {} [{verdict}]",
        verdict_sentence(verdict, &to)
    ))
}

fn tool_broadcast_notify(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let circle = args.get("circle").and_then(|v| v.as_str());
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let peers = store.list_peers().map_err(e)?;
    let online: Vec<String> = peers
        .into_iter()
        .filter(|p| {
            circle
                .map(|c| model::circle_or_default(&p.circle) == c)
                .unwrap_or(true)
        })
        .filter(|p| p.name != from)
        .filter(store::is_alive)
        .map(|p| p.name)
        .collect();
    if online.is_empty() {
        return Ok("No online peers in circle to notify.".to_string());
    }
    let mut lines = Vec::new();
    for peer in &online {
        let trace_id = Some(model::mint_trace_id());
        let mid = store
            .send(
                &from,
                peer,
                subject.as_deref(),
                body,
                None,
                trace_id.as_deref(),
            )
            .map_err(e)?;
        if let Some(p) = priority {
            let _ = store.set_message_priority(mid, p);
        }
        let verdict = ask_delivery_verdict(store, nudge_template, &from, peer, body, injector);
        let (stage, outcome) = verdict_to_stage(verdict);
        record_delivery_best_effort(
            store,
            mid,
            model::DeliveryRefKind::Notify,
            peer,
            stage,
            outcome,
        );
        lines.push(format!("{peer}: #{mid} [{verdict}]"));
    }
    Ok(format!(
        "Broadcast-notified {} peer(s):\n{}",
        online.len(),
        lines.join("\n")
    ))
}

fn tool_broadcast_ask(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let circle = args.get("circle").and_then(|v| v.as_str());
    let _reply_to = args.get("reply_to").and_then(|v| v.as_i64());
    let peers = store.list_peers().map_err(e)?;
    let online: Vec<String> = peers
        .into_iter()
        .filter(|p| {
            circle
                .map(|c| model::circle_or_default(&p.circle) == c)
                .unwrap_or(true)
        })
        .filter(|p| p.name != from)
        .filter(store::is_alive)
        .map(|p| p.name)
        .collect();
    if online.is_empty() {
        return Ok("No online peers in circle to ask.".to_string());
    }
    let outcome = store
        .create_ask_many(&from, &online, subject.as_deref(), body)
        .map_err(e)?;
    let mut lines = Vec::new();
    for (peer, res) in &outcome.children {
        match res {
            Ok(cid) => {
                let verdict =
                    ask_delivery_verdict(store, nudge_template, &from, peer, body, injector);
                lines.push(format!("{peer}: {cid} ({verdict})"));
            }
            Err(err) => {
                lines.push(format!("{peer}: FAILED ({err})"));
            }
        }
    }
    Ok(format!(
        "Broadcast-ask {} ({} created):\n{}",
        outcome.parent_id,
        outcome.children.iter().filter(|(_, r)| r.is_ok()).count(),
        lines.join("\n")
    ))
}

/// `weave_delivery`: show the DELIVERY/transport trace for a message
/// (queued → injected/inject_failed/not_injectable → drained). The transport-side
/// complement to `weave_receipts` (which shows READ receipts). Read-only,
/// metadata-only — the trace carries NO body/subject. An unknown/never-traced id is
/// NOT an error (returns the empty-trace line).
fn tool_delivery(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("message_id")
        .and_then(|v| v.as_i64())
        .ok_or("'message_id' is required (the message id to trace).")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(model::MAX_DELIVERY_ROWS);
    let trace = store.list_delivery(id, limit).map_err(e)?;
    if trace.is_empty() {
        return Ok(format!("No delivery trace for #{id}."));
    }
    let mut out = format!("Delivery trace for #{id} ({} stage(s)):\n", trace.len());
    for t in &trace {
        out.push_str(&format!(
            "[{}] {}/{} -> {} ({})\n",
            fmt_ts(t.ts),
            t.stage,
            t.outcome,
            t.to_peer,
            t.ref_kind
        ));
    }
    Ok(out)
}

/// List pending cross-store intents in this store's outbox (Tier-2, read-only).
fn tool_outbox(store: &dyn Store, args: &Value) -> Result<String, String> {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(200);
    let intents = store.outbox_all(limit).map_err(e)?;
    if intents.is_empty() {
        return Ok("Outbox empty (no pending cross-store intents).".to_string());
    }
    let mut out = format!("{} pending intent(s):\n", intents.len());
    for i in &intents {
        let subj = i
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        let host = if i.to_host.is_empty() {
            String::new()
        } else {
            format!("@{}", i.to_host)
        };
        out.push_str(&format!(
            "#{} {} -> {}{}{}: {}\n",
            i.id, i.from, i.to, host, subj, i.body
        ));
    }
    Ok(out)
}

fn tool_inbox(
    store: &dyn Store,
    def: &Option<String>,
    pull: &PullConsent,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    // Tier-2: pull cross-store intents into the local inbox BEFORE reading, so a
    // federated message is delivered in this same drain. Best-effort — a pull
    // failure must not fail the inbox read; diagnostics go to stderr (never
    // stdout, which carries only JSON-RPC frames).
    if !pull.from.is_empty() {
        match store::pull_from_store(store, &me, &pull.from, &pull.policy) {
            Ok(p) if p.committed > 0 => {
                log(&format!(
                    "pulled {} cross-store message(s) for '{me}'",
                    p.committed
                ));
                // Decision-5 consent nudge (DEFAULT ON), fired CALLER-SIDE here in
                // `mcp` (which depends on both `store` and `inject`) so
                // `pull_from_store` never gains a `store → inject` edge. Diagnostics
                // → stderr only (stdout carries JSON-RPC frames). Best-effort.
                nudge_pulled(store, pull, &me, &p.committed_sources, injector);
            }
            Ok(_) => {}
            Err(err) => log(&format!("pull skipped (non-fatal): {err}")),
        }
    }
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

/// Tier-2 consent nudge (decision 5, DEFAULT ON) for the MCP inbox drain: after a
/// pull commits cross-store messages, fire the EXISTING paste-safe content-free
/// [`Nudge::Nudge`] into THIS session's OWN registered pane (never a
/// foreign pane, never the body). Mirrors `main::nudge_pulled`. Done caller-side
/// so `store::pull_from_store` never gains a `store → inject` edge.
///
/// Gating: no-op unless `pull.inject_pulled` is on (the single off-switch ⇒ pure
/// queue-only) AND at least one committed source passes `inject_allowed_from`.
/// Falls back silently to queue-only when this session's pane is not injectable
/// (`mux=none`) or not alive. Best-effort: any failure is logged to STDERR (never
/// stdout, which carries only JSON-RPC frames) and never breaks the drain.
fn nudge_pulled(
    store: &dyn Store,
    pull: &PullConsent,
    me: &str,
    committed_sources: &[StoreSource],
    injector: &dyn Injector,
) {
    if !pull.inject_pulled {
        return;
    }
    if !committed_sources
        .iter()
        .any(|src| pull.inject_allowed_from(src))
    {
        return;
    }
    let peer = match store.get_peer(me) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(err) => {
            log(&format!("pull-nudge skipped (non-fatal): {err}"));
            return;
        }
    };
    let target = Target::from_peer(&peer);
    if !target.injectable() || !injector.target_alive(&target) {
        return;
    }
    match injector.inject_mode(&target, "", Nudge::Nudge) {
        Ok(_) => {}
        Err(err) => log(&format!("pull-nudge inject failed (non-fatal): {err}")),
    }
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

fn tool_search(store: &dyn Store, args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Missing or empty 'query' parameter.".to_string())?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let rows = store.search(query, limit).map_err(e)?;
    if rows.is_empty() {
        return Ok(format!("Search for '{query}': no matches."));
    }
    let mut out = format!("Search ('{query}') — {} message(s):", rows.len());
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

fn tool_sessions(
    store: &dyn Store,
    def: &Option<String>,
    extra_dbs: &[StoreSource],
    args: &Value,
) -> Result<String, String> {
    let me = def.clone().unwrap_or_default();
    // Tier-1 federation: union local sessions with read-only extra stores,
    // origin-tagged. Default (no extra stores) ⇒ the local listing unchanged.
    let mut info = store::federated_sessions(store, extra_dbs).map_err(e)?;
    let total = store.total_messages().map_err(e)?;
    if info.is_empty() {
        return Ok("No sessions seen yet — the store is empty.".into());
    }
    // Display-layer tag join (purely additive, no schema/trait/federation change):
    // SessionView is message-derived and carries no git tags, so we look up the LOCAL
    // peer by session name and attach its repo/branch/worktree for display only. Only
    // the local store's peers are consulted (never foreign rows); a session without a
    // registered peer simply renders no tags.
    let local_peers: std::collections::HashMap<String, weave_core::model::Peer> = store
        .list_peers()
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();
    // P4 circle scope: a session's circle is its registered local peer's circle (a
    // session with no peer row classifies as "default"). With everyone in "default"
    // and no arg this keeps every row.
    if let Some(target) = resolve_mcp_circle(store, def, args).as_deref() {
        info.retain(|v| {
            let c = local_peers
                .get(&v.name)
                .map(|p| p.circle.as_str())
                .unwrap_or("");
            weave_core::model::circle_or_default(c) == target
        });
        if info.is_empty() {
            return Ok(format!("No sessions in circle '{target}'."));
        }
    }
    let mut out = format!("Known sessions ({}), {total} message(s) total:", info.len());
    for v in info {
        let mine = if !me.is_empty() && v.name == me {
            "  <- you"
        } else {
            ""
        };
        let via = if v.origin.is_foreign() {
            format!(" (via {})", v.origin.label())
        } else {
            String::new()
        };
        let tags = local_peers
            .get(&v.name)
            .map(fmt_peer_tags)
            .unwrap_or_default();
        out.push_str(&format!(
            "\n  • {}: {} unread (last activity {}){tags}{via}{mine}",
            v.name,
            v.unread,
            fmt_ts(v.last_activity)
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

/// Resolve the effective circle for an MCP `weave_peers`/`weave_sessions`/
/// `weave_scan` listing (P4), returning `None` for "no filter" (mesh-wide).
/// Mirrors the CLI `resolve_list_circle`: an explicit `circle` arg (`"*"` ⇒
/// mesh-wide) wins; else an orchestrator caller goes mesh-wide; else the caller's
/// own configured circle. With everyone in `"default"` and no arg this returns
/// `Some("default")` ⇒ byte-identical to pre-P4.
fn resolve_mcp_circle(store: &dyn Store, def: &Option<String>, args: &Value) -> Option<String> {
    if let Some(c) = args
        .get("circle")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if c == "*" {
            return None;
        }
        return Some(weave_core::model::circle_or_default(c).to_string());
    }
    if let Some(d) = def.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Ok(Some(p)) = store.get_peer(d.trim()) {
            if weave_core::model::PeerRole::from_str(&p.role)
                == Ok(weave_core::model::PeerRole::Orchestrator)
            {
                return None;
            }
        }
    }
    Some(weave_core::config::Config::load().circle())
}

fn tool_peers(
    store: &dyn Store,
    def: &Option<String>,
    extra_dbs: &[StoreSource],
    args: &Value,
    _injector: &dyn Injector,
) -> Result<String, String> {
    // Tier-1 federation: union local peers with read-only extra stores,
    // origin-tagged. Default (no extra stores) ⇒ the local listing unchanged.
    let mut views = store::federated_peers(store, extra_dbs).map_err(e)?;
    // P4 circle scope (caller-side filter; federation composes).
    if let Some(target) = resolve_mcp_circle(store, def, args).as_deref() {
        views.retain(|v| weave_core::model::circle_or_default(&v.peer.circle) == target);
    }
    if views.is_empty() {
        return Ok("No peers registered yet. Sessions register via `weave hook session`.".into());
    }
    // Host-aware liveness reason per peer (A2 vocabulary, display-only); mirrors
    // `weave scan` / `weave_scan`. Never a cross-machine probe; secret-free.
    let this_host = weave_core::config::this_host();
    let now_ts = weave_core::model::now();
    let mut out = format!("Registered peers ({}):", views.len());
    for v in views {
        let p = &v.peer;
        let inj = if Target::from_peer(p).injectable() {
            "injectable"
        } else {
            "no-inject"
        };
        let presence = if is_alive(p) { "online" } else { "offline" };
        let liveness = store::liveness_for(p, &this_host, now_ts);
        let reason = match liveness {
            store::Liveness::AliveLocal if p.pid.is_some() => "alive (local, pid)",
            store::Liveness::AliveLocal => "alive (local, ttl)",
            store::Liveness::AliveRemote => "alive (remote, ttl)",
            store::Liveness::Stale => "stale",
        };
        let diag = mcp_peer_diagnostics(store, p, liveness, &this_host, now_ts);
        let remote_marker = if p.host != this_host { " <remote>" } else { "" };
        let via = if v.origin.is_foreign() {
            format!(" (via {})", v.origin.label())
        } else {
            String::new()
        };
        let tags = fmt_peer_tags(p);
        let ts_marker = fmt_turn_state(p);
        let desc = fmt_description(p);
        out.push_str(&format!(
            "\n  • {}{remote_marker} [{presence}] [{reason}]{ts_marker} [{}] {} ({inj}){tags}{desc} seen {} process_alive={} pane_alive={} reachable={} stale_reason={} last_transport_success={} last_response={} inject_probe={}{via}",
            p.name,
            p.mux,
            if p.target.is_empty() { "-" } else { &p.target },
            fmt_ts(p.last_seen),
            diag.process_alive,
            diag.pane_alive,
            diag.reachable,
            if diag.stale_reason.is_empty() { "-" } else { diag.stale_reason },
            diag.last_transport_success,
            diag.last_response,
            diag.inject_probe,
        ));
    }
    Ok(out)
}

/// Render a peer's git session tags for an MCP listing, e.g. ` {weave@feat/x
/// #my-wt}`, omitting empty fields and the whole group for a non-git session.
/// Mirrors the CLI `fmt_peer_tags`. Pure formatting.
fn fmt_peer_tags(p: &weave_core::model::Peer) -> String {
    if p.repo.is_empty() && p.branch.is_empty() && p.worktree_id.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    match (p.repo.is_empty(), p.branch.is_empty()) {
        (false, false) => parts.push(format!("{}@{}", p.repo, p.branch)),
        (false, true) => parts.push(p.repo.clone()),
        (true, false) => parts.push(format!("@{}", p.branch)),
        (true, true) => {}
    }
    if !p.worktree_id.is_empty() {
        parts.push(format!("#{}", p.worktree_id));
    }
    format!(" {{{}}}", parts.join(" "))
}

/// Compact, NON-NOISY turn_state marker for an MCP listing (P5), e.g. ` [working]`.
/// An idle/unknown turn_state renders nothing (so a pre-P5 peer's line is unchanged);
/// mirrors the CLI `fmt_turn_state`. Pure formatting.
fn fmt_turn_state(p: &weave_core::model::Peer) -> String {
    match weave_core::model::TurnState::from_str(&p.turn_state) {
        Ok(weave_core::model::TurnState::Working) => " [working]".to_string(),
        Ok(weave_core::model::TurnState::AwaitingInput) => " [awaiting-input]".to_string(),
        Ok(weave_core::model::TurnState::PendingFirstTurn) => " [pending]".to_string(),
        _ => String::new(),
    }
}

/// Compact description suffix for an MCP listing (P5), e.g. ` "reviewing PR #23"`.
/// An empty (unset/TTL-expired) description renders nothing. The Peer is expected to
/// carry the read-time-TTL'd view from the store. Pure formatting.
fn fmt_description(p: &weave_core::model::Peer) -> String {
    if p.description.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", p.description)
    }
}

#[derive(Debug, Clone)]
struct McpPeerDiagnostics {
    process_expected: bool,
    process_alive: bool,
    pane_alive: bool,
    reachable: bool,
    responsive_recently: bool,
    last_transport_success: i64,
    last_response: i64,
    stale_reason: &'static str,
    inject_probe: &'static str,
}

fn mcp_peer_recently_responded(store: &dyn Store, name: &str, now_ts: i64) -> bool {
    store
        .list_asks(name, weave_core::model::AskRole::Askee, 50)
        .unwrap_or_default()
        .into_iter()
        .any(|a| {
            matches!(
                a.state,
                weave_core::model::AskState::Answered | weave_core::model::AskState::Acked
            ) && now_ts.saturating_sub(a.updated_ts) <= 15 * 60
        })
}

fn mcp_peer_last_response(store: &dyn Store, name: &str) -> i64 {
    store
        .list_asks(name, weave_core::model::AskRole::Askee, 200)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| {
            matches!(
                a.state,
                weave_core::model::AskState::Answered | weave_core::model::AskState::Acked
            )
        })
        .map(|a| a.updated_ts)
        .max()
        .unwrap_or(0)
}

fn mcp_peer_last_transport_success(store: &dyn Store, name: &str) -> i64 {
    store
        .history(name, None, 200)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|m| {
            store
                .list_delivery(m.id, weave_core::model::MAX_DELIVERY_ROWS)
                .unwrap_or_default()
        })
        .filter(|t| {
            t.to_peer == name
                && t.outcome == weave_core::model::DeliveryOutcome::Ok.as_str()
                && (t.stage == weave_core::model::DeliveryStage::Injected.as_str()
                    || t.stage == weave_core::model::DeliveryStage::Drained.as_str())
        })
        .map(|t| t.ts)
        .max()
        .unwrap_or(0)
}

fn mcp_peer_diagnostics(
    store: &dyn Store,
    p: &weave_core::model::Peer,
    liveness: store::Liveness,
    this_host: &str,
    now_ts: i64,
) -> McpPeerDiagnostics {
    let target = Target::from_peer(p);
    let capability = weave_inject::capability(&target);
    let process_expected = p.pid.is_some() && p.host == this_host;
    let process_alive = match p.pid {
        Some(pid) if process_expected => store::pid_alive(pid),
        _ => false,
    };
    let stale_reason = if !matches!(liveness, store::Liveness::Stale) {
        ""
    } else if process_expected && !process_alive {
        "process_dead"
    } else if !store::is_online_at(p.last_seen, now_ts) {
        "heartbeat_stale"
    } else {
        "stale"
    };
    McpPeerDiagnostics {
        process_expected,
        process_alive,
        pane_alive: matches!(capability, Capability::Live),
        reachable: matches!(capability, Capability::Live),
        responsive_recently: mcp_peer_recently_responded(store, &p.name, now_ts),
        last_transport_success: mcp_peer_last_transport_success(store, &p.name),
        last_response: mcp_peer_last_response(store, &p.name),
        stale_reason,
        inject_probe: match capability {
            Capability::Live => "live",
            Capability::RegisteredNotAlive => "absent",
            Capability::NotInjectable => "not_injectable",
        },
    }
}

/// `weave_scan`: refresh THIS session's own row tags, then list every (federated)
/// peer joined with liveness and its repo/branch/worktree tags, with optional
/// `repo`/`branch` exact-match filters. OWNER-ONLY-WRITES: the self-refresh only
/// ever re-registers the caller's own row (under `me_default`), never a foreign
/// one. All output is the returned tool-result TEXT; capture skip-notes go to
/// stderr (MCP stdout carries only JSON-RPC frames). A missing/blank default
/// identity simply skips the self-refresh (read-only scan) rather than erroring.
fn tool_scan(
    store: &dyn Store,
    def: &Option<String>,
    extra_dbs: &[StoreSource],
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    // Self-refresh (owner-only-writes), best-effort: a failure is noted to STDERR
    // and never aborts the read.
    if let Some(me) = def.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Ok(me) = bound_ident("me", me) {
            let t = injector.detect_target();
            let tags = injector.git_tags_here();
            if let Err(err) = store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                None,
                Some(std::process::id() as i64),
                &weave_core::config::this_host(),
                &tags.repo,
                &tags.branch,
                &tags.worktree_id,
                &weave_core::config::Config::load().circle(),
                None,
                "",
            ) {
                eprintln!("[weave] scan self-refresh skipped (non-fatal): {err}");
            }
        }
    }

    // Optional exact-match filters, each bounded so a hostile/oversized filter arg
    // is rejected gracefully (never a panic) before it touches the listing.
    let repo_filter = match args.get("repo").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => Some(bound_ident("repo", s)?),
        _ => None,
    };
    let branch_filter = match args.get("branch").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => Some(bound_ident("branch", s)?),
        _ => None,
    };

    let mut views = store::federated_peers(store, extra_dbs).map_err(e)?;
    if let Some(r) = repo_filter.as_deref() {
        views.retain(|v| v.peer.repo == r);
    }
    if let Some(b) = branch_filter.as_deref() {
        views.retain(|v| v.peer.branch == b);
    }
    // P4 circle scope (caller-side filter; federation composes).
    if let Some(target) = resolve_mcp_circle(store, def, args).as_deref() {
        views.retain(|v| weave_core::model::circle_or_default(&v.peer.circle) == target);
    }
    if views.is_empty() {
        return Ok("No peers match the scan.".into());
    }
    // Host-aware liveness reason per row (pure A2 reinterpretation of the
    // read-only federated rows; never a cross-machine probe). Mirrors `weave scan`.
    let this_host = weave_core::config::this_host();
    let now_ts = weave_core::model::now();
    let mut out = format!("Scan ({} peer(s)):", views.len());
    let mut local_alive = 0usize;
    let mut remote_alive = 0usize;
    let mut stale = 0usize;
    for v in views {
        let p = &v.peer;
        let liveness = store::liveness_for(p, &this_host, now_ts);
        let reason = match liveness {
            store::Liveness::AliveLocal if p.pid.is_some() => "alive (local, pid)",
            store::Liveness::AliveLocal => "alive (local, ttl)",
            store::Liveness::AliveRemote => "alive (remote, ttl)",
            store::Liveness::Stale => "stale",
        };
        let diag = mcp_peer_diagnostics(store, p, liveness, &this_host, now_ts);
        match liveness {
            store::Liveness::AliveLocal => local_alive += 1,
            store::Liveness::AliveRemote => remote_alive += 1,
            store::Liveness::Stale => stale += 1,
        }
        let remote_marker = if p.host != this_host { " <remote>" } else { "" };
        let via = if v.origin.is_foreign() {
            format!(" (via {})", v.origin.label())
        } else {
            String::new()
        };
        let ts_marker = fmt_turn_state(p);
        let desc = fmt_description(p);
        out.push_str(&format!(
            "\n  • {}{remote_marker} [{reason}]{ts_marker} repo={} branch={} worktree={} mux={} pane={} host={}{desc} process_alive={} pane_alive={} reachable={} responsive={} stale_reason={} last_transport_success={} last_response={} inject_probe={}{via}",
            p.name,
            if p.repo.is_empty() { "-" } else { &p.repo },
            if p.branch.is_empty() { "-" } else { &p.branch },
            if p.worktree_id.is_empty() {
                "-"
            } else {
                &p.worktree_id
            },
            p.mux,
            if p.target.is_empty() { "-" } else { &p.target },
            if p.host.is_empty() { "-" } else { &p.host },
            diag.process_alive,
            diag.pane_alive,
            diag.reachable,
            diag.responsive_recently,
            if diag.stale_reason.is_empty() { "-" } else { diag.stale_reason },
            diag.last_transport_success,
            diag.last_response,
            diag.inject_probe,
        ));
    }
    out.push_str(&format!(
        "\nsummary: {local_alive} local-alive, {remote_alive} remote-alive, {stale} stale"
    ));
    Ok(out)
}

/// `weave_claim_orchestrator` (P4): claim the per-circle orchestrator slot for the
/// caller. `{from?, circle?, force?}`. A live-holder-without-force is a clean
/// REFUSAL (a normal tool result, not a protocol error). An unregistered caller is
/// an error. NO injector involvement (a role is a pure DB bit). All output is the
/// returned tool-result TEXT; any diagnostics go to stderr.
fn tool_claim_orchestrator(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "from", def)?;
    let circle = args
        .get("circle")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    match store.claim_orchestrator_role(&me, circle, force).map_err(e)? {
        weave_core::model::ClaimOutcome::Claimed { circle, demoted } => {
            let mut out = format!("claimed role=orchestrator for '{me}' in circle '{circle}'");
            if !demoted.is_empty() {
                out.push_str(&format!(" (demoted: {})", demoted.join(", ")));
            }
            Ok(out)
        }
        weave_core::model::ClaimOutcome::Refused { circle, holder } => Ok(format!(
            "refused: '{holder}' is the live orchestrator in circle '{circle}' (pass force=true to steal)"
        )),
    }
}

/// `weave_orchestrator_status` (P4): report the live orchestrator of a circle.
/// `{circle?}` (omitted ⇒ the caller's configured circle). "live" reuses the
/// store's `is_alive` verdict (no new probe). Secret-free; tool-result TEXT only.
fn tool_orchestrator_status(store: &dyn Store, args: &Value) -> Result<String, String> {
    let circle = args
        .get("circle")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| weave_core::config::Config::load().circle());
    let st = store.orchestrator_status(Some(&circle)).map_err(e)?;
    if st.present {
        let names: Vec<_> = st.holders.iter().map(|h| h.name.as_str()).collect();
        Ok(format!(
            "orchestrator(s) present in circle '{}': {} (online)",
            st.circle,
            names.join(", ")
        ))
    } else {
        Ok(format!("no live orchestrator in circle '{}'", st.circle))
    }
}

/// Reply to an existing message. The recipient is derived by the store from the
/// parent message (it addresses the reply back to the parent's other party), so
/// the caller supplies only their identity, the parent id, and the body.
fn tool_reply(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let in_reply_to = args
        .get("in_reply_to")
        .and_then(|v| v.as_i64())
        .ok_or("'in_reply_to' is required (the message id you're replying to).")?;
    if in_reply_to <= 0 {
        return Err("'in_reply_to' must be a positive message id.".into());
    }
    let no_memory = args
        .get("no_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body_raw = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let body = maybe_prefix_body_mcp(&from, body_raw, no_memory);
    // WL-038: optional ephemeral TTL, cap-validated at the seam.
    let ttl = match args.get("ttl").and_then(|v| v.as_i64()) {
        Some(t) if !model::ttl_valid(t) => {
            return Err(format!(
                "'ttl' must be between 1 and {} seconds.",
                model::MAX_MSG_TTL_SECS
            ));
        }
        other => other,
    };

    let mid = store.reply(&from, in_reply_to, &body).map_err(e)?;
    // WL-038: post-stamp the ephemeral expiry after persist.
    if let Some(t) = ttl {
        let _ = store.set_message_expiry(mid, model::expiry_from_ttl(model::now(), t));
    }
    let mut out = format!("Replied to #{in_reply_to} as message #{mid} from '{from}'.");

    // Native push: nudge the reply's recipient if it resolved to a registered
    // injectable peer. The recipient is exactly what `reply_target` derived from
    // the parent (no need to re-scan the whole thread, which also capped at
    // MAX_LIMIT and could miss deep threads).
    if let Ok((to, _subject)) = store.reply_target(&from, in_reply_to) {
        if !model::is_broadcast(&to) {
            if let Ok(Some(peer)) = store.get_peer(&to) {
                let target = Target::from_peer(&peer);
                if target.injectable() {
                    let (nudge, mode) = build_nudge(nudge_template, &from, &body);
                    match injector.inject_mode(&target, &nudge, mode) {
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
fn tool_doctor(
    store: &dyn Store,
    extra_dbs: &[StoreSource],
    injector: &dyn Injector,
) -> Result<String, String> {
    let target = injector.detect_target();
    // Tier-1 federation: report the union peer count (local + read-only extras).
    let views = store::federated_peers(store, extra_dbs).map_err(e)?;
    let total_peers = views.len();
    let online = views.iter().filter(|v| is_alive(&v.peer)).count();
    // Host-aware liveness breakdown over the peer set (A2 vocabulary, display-only),
    // mirroring `weave doctor`. Deterministic given this_host/now; secret-free.
    let this_host = weave_core::config::this_host();
    let now_ts = weave_core::model::now();
    let mut peers_alive_local = 0usize;
    let mut peers_alive_remote = 0usize;
    let mut peers_stale = 0usize;
    let mut process_expected = 0usize;
    let mut process_alive = 0usize;
    let mut pane_alive = 0usize;
    let mut reachable = 0usize;
    let mut responsive = 0usize;
    let mut transport_seen = 0usize;
    let mut response_seen = 0usize;
    for v in &views {
        let liveness = store::liveness_for(&v.peer, &this_host, now_ts);
        match liveness {
            store::Liveness::AliveLocal => peers_alive_local += 1,
            store::Liveness::AliveRemote => peers_alive_remote += 1,
            store::Liveness::Stale => peers_stale += 1,
        }
        let diag = mcp_peer_diagnostics(store, &v.peer, liveness, &this_host, now_ts);
        process_expected += usize::from(diag.process_expected);
        process_alive += usize::from(diag.process_alive);
        pane_alive += usize::from(diag.pane_alive);
        reachable += usize::from(diag.reachable);
        responsive += usize::from(diag.responsive_recently);
        transport_seen += usize::from(diag.last_transport_success > 0);
        response_seen += usize::from(diag.last_response > 0);
    }
    let (fed_ok, fed_skipped) = store::federation_status(extra_dbs);
    let total = store.total_messages().map_err(e)?;
    let claude = injector.have("claude");
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
        "\n  peers:          {total_peers} ({online} online)"
    ));
    out.push_str(&format!(
        "\n  liveness:       {peers_alive_local} local-alive, {peers_alive_remote} remote-alive, {peers_stale} stale"
    ));
    out.push_str(&format!(
        "\n  dimensions:     registered={}, process_alive={}/{}, pane_alive={}, reachable={}, responsive={}, transport_seen={}, response_seen={}",
        total_peers,
        process_alive,
        process_expected,
        pane_alive,
        reachable,
        responsive,
        transport_seen,
        response_seen,
    ));
    out.push_str(&format!(
        "\n  claude on PATH: {}",
        if claude { "yes" } else { "no" }
    ));
    if !extra_dbs.is_empty() {
        out.push_str(&format!(
            "\n  federation:     {} extra store(s) ({fed_ok} ok, {fed_skipped} skipped)",
            extra_dbs.len()
        ));
        let remote_count = extra_dbs.iter().filter(|s| s.is_remote()).count();
        if remote_count > 0 {
            out.push_str(&format!("\n  remote sources: {remote_count} configured"));
            // Token-FREE per-source token-tier observability, consistent with the CLI
            // `weave doctor`. NEVER prints any token byte — only aggregate counts.
            let tiers = weave_core::config::Config::load().peer_db_remote_token_tiers();
            let per_source = tiers
                .iter()
                .filter(|t| **t == weave_core::config::PullTokenTier::PerSourceLabel)
                .count();
            let shared = tiers
                .iter()
                .filter(|t| **t == weave_core::config::PullTokenTier::Shared)
                .count();
            let none = tiers
                .iter()
                .filter(|t| **t == weave_core::config::PullTokenTier::None)
                .count();
            out.push_str(&format!(
                "\n  remote tokens:  {per_source} per-source, {shared} shared, {none} none"
            ));
            // Token-FREE per-source TIMEOUT-tier observability, parity with the CLI
            // `weave doctor`. Only aggregate tier counts + an effective ms range; never
            // a token byte. The result string is the JSON-RPC tool RESULT (stdout
            // frame); all skip/timeout diagnostics stay on stderr.
            let timeout_tiers = weave_core::config::Config::load().peer_db_remote_timeout_tiers();
            let t_per_source = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == weave_core::config::PullTimeoutTier::PerSourceLabel)
                .count();
            let t_global = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == weave_core::config::PullTimeoutTier::Global)
                .count();
            let t_default = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == weave_core::config::PullTimeoutTier::Default)
                .count();
            let tmin = timeout_tiers.iter().map(|(ms, _)| *ms).min().unwrap_or(0);
            let tmax = timeout_tiers.iter().map(|(ms, _)| *ms).max().unwrap_or(0);
            out.push_str(&format!(
                "\n  remote timeout: {t_per_source} per-source, {t_global} global, {t_default} default (effective {tmin}-{tmax} ms)"
            ));
            if !cfg!(feature = "libsql") {
                out.push_str(&format!(
                    "\n  note: {remote_count} remote source(s) skipped — rebuild weave with --features libsql to use them"
                ));
            }
        }
    }
    // Additive secret-free "federation health" rollup for the `pull_from` delivery
    // set — parity with the CLI `weave doctor` (the side never surfaced before).
    // Counts/tiers only; never a token byte. Reads config/env via the same
    // `Config::load()` pattern the peer_db tiers above use (the full Config isn't
    // plumbed into the MCP server). The whole string is the JSON-RPC RESULT (stdout
    // frame); no skip/timeout note is emitted here, so stdout discipline holds.
    let ph = weave_core::config::Config::load()
        .federation_health()
        .pull_from;
    if ph.total > 0 {
        out.push_str(&format!(
            "\n  pull sources:   {} configured ({} local, {} remote)",
            ph.total, ph.local, ph.remote
        ));
        if ph.remote > 0 {
            out.push_str(&format!(
                "\n  pull tokens:    {} per-source, {} shared, {} none",
                ph.token_per_source, ph.token_shared, ph.token_none
            ));
            let (pmin, pmax) = (ph.ms_min.unwrap_or(0), ph.ms_max.unwrap_or(0));
            out.push_str(&format!(
                "\n  pull timeout:   {} per-source, {} global, {} default (effective {pmin}-{pmax} ms)",
                ph.timeout_per_source, ph.timeout_global, ph.timeout_default
            ));
        }
    }
    // Signed-identity verify summary (parity with the CLI `weave doctor` human block).
    // Counts + this session's OWN fingerprint only — NEVER a peer pubkey, a token, or
    // the private key. Reads trust/revoked policy via `Config::load()` (the full Config
    // is not plumbed into the MCP server; same pattern as the federation rollup above)
    // and the multi-key registry / revocation log via `store`. The whole `out` string
    // is the JSON-RPC tool RESULT (stdout frame); no diagnostic is emitted to stdout,
    // so stdout discipline holds (any logging stays on stderr elsewhere).
    #[cfg(feature = "sign")]
    {
        let cfg = weave_core::config::Config::load();
        let trust = cfg.trust_set();
        let revoked = cfg.revoked_set();
        let mode = match cfg.strict_verify_override() {
            Some(true) => "forced",
            Some(false) => "disabled",
            None => "default (trust-set aware)",
        };
        out.push_str(&format!(
            "\n  signed id:      strict={mode}, trusted={}, revoked={}",
            trust.len(),
            revoked.len()
        ));
        if let Ok(pairs) = store.list_keys() {
            use std::collections::BTreeSet;
            let idents: BTreeSet<&str> = pairs.iter().map(|(i, _)| i.as_str()).collect();
            let mut multi = std::collections::BTreeMap::<&str, usize>::new();
            for (i, _) in &pairs {
                *multi.entry(i.as_str()).or_insert(0) += 1;
            }
            let mid_rotation = multi.values().filter(|&&c| c > 1).count();
            out.push_str(&format!(
                "\n  key registry:   {} identities, {} keys ({mid_rotation} mid-rotation)",
                idents.len(),
                pairs.len()
            ));
            let hit = pairs
                .iter()
                .filter(|(_, pk)| revoked.iter().any(|e| sign::fingerprint_matches(e, pk)))
                .count();
            out.push_str(&format!(
                "\n  revoked keys:   {hit} registered key(s) currently revoked"
            ));
        }
        if let Ok(events) = store.count_revocations() {
            out.push_str(&format!("\n  revocation log: {events} event(s) recorded"));
        }
        let local_fp = sign::local_public_key()
            .ok()
            .flatten()
            .and_then(|pk| sign::fingerprint(&pk))
            .unwrap_or_else(|| "none".to_string());
        out.push_str(&format!("\n  my fingerprint: {local_fp}"));
    }
    out.push_str("\n  (db/config paths: run `weave doctor` on the CLI)");
    // FR6: warn when the resolved store is NOT the well-known XDG default — the most
    // common "why can't I see the other session's peers" cause is a mismatched
    // WEAVE_DB. Compare against the same default `Config::db_path` derives from.
    let db = weave_core::config::Config::load().db_path();
    let db_default = weave_core::config::default_db_path();
    if db != db_default {
        out.push_str(&format!(
            "\n  note: using non-default WEAVE_DB ({}) — peers on a different store won't be visible.",
            db.display()
        ));
    }
    Ok(out)
}

/// Echo the resolved identity (default session) and the active storage backend,
/// plus how the current process would inject. Lets a caller confirm "who am I"
/// before sending.
fn tool_whoami(
    store: &dyn Store,
    def: &Option<String>,
    injector: &dyn Injector,
) -> Result<String, String> {
    let identity = match def {
        Some(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => "(unset — pass 'from'/'me' explicitly)".to_string(),
    };
    let target = injector.detect_target();
    let tgt = if target.id.is_empty() {
        "-"
    } else {
        &target.id
    };
    // P4: report the caller's resolved circle (config/$WEAVE_CIRCLE) and its
    // current registered role, so a caller can confirm its visibility scope. P5:
    // also surface the caller's own turn_state + description (read-time-TTL'd by the
    // store). One peer-row lookup feeds role/turn_state/description; a missing row
    // falls back to the defaults.
    let circle = weave_core::config::Config::load().circle();
    let me_row = match def {
        Some(d) if !d.trim().is_empty() => store.get_peer(d.trim()).ok().flatten(),
        _ => None,
    };
    let role = me_row
        .as_ref()
        .and_then(|p| weave_core::model::PeerRole::from_str(&p.role).ok())
        .unwrap_or(weave_core::model::PeerRole::Peer)
        .as_str()
        .to_string();
    // whoami is a verbose self-report (noise is fine), so turn_state/description are
    // ALWAYS shown — `-` when unset (Unknown / empty / TTL-expired).
    let turn_state = me_row
        .as_ref()
        .and_then(|p| weave_core::model::TurnState::from_str(&p.turn_state).ok())
        .map(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("-")
        .to_string();
    let description = me_row
        .as_ref()
        .map(|p| p.description.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "-".to_string());
    Ok(format!(
        "identity:   {identity}\nbackend:    {}\ncircle:     {circle}\nrole:       {role}\nturn_state: {turn_state}\ndescription: {description}\nthis pane:  mux={} target={} injectable={}",
        store.backend(),
        target.mux.as_str(),
        tgt,
        target.injectable()
    ))
}

/// Zero-restart adoption: re-capture THIS process's pane env and upsert the
/// caller's OWN peer row (idempotent `ON CONFLICT(name) DO UPDATE`). The row key
/// is bound to the resolved caller identity (`me`/default session), never an
/// arbitrary arg-supplied target, so attach can only ever (re)register the caller
/// itself — it can never overwrite another session's row.
///
/// Note (env semantics): `detect_target()` reads the MCP server process's env,
/// i.e. the agent's mux env captured when it spawned `weave mcp`. If an agent
/// re-parents panes mid-session, the CLI `weave attach` (run inside the live pane)
/// is the authoritative path.
fn tool_attach(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    // Resolve + validate the caller's own identity (this is the row key).
    let me = ident(args, "me", def)?;
    let t = injector.detect_target();
    // A detected mux must carry a structurally valid pane id, or we refuse to
    // persist a poisoned, un-injectable registration. A legitimate mux=none has an
    // empty id and is allowed (store-only delivery).
    if t.injectable() && !injector.id_valid(t.mux, &t.id) {
        return Err(format!(
            "refusing to attach: captured target {:?} is not a valid {} target.",
            t.id,
            t.mux.as_str()
        ));
    }
    // Capture the MCP server process's PID + host so the adopted peer reflects
    // real liveness (this is the agent's own process), plus the git session tags
    // derived from the server's cwd (best-effort; a git failure ⇒ empty tags).
    let tags = injector.git_tags_here();
    let cert = store
        .register_peer_full(
            &me,
            t.mux.as_str(),
            &t.id,
            &t.socket,
            None,
            Some(std::process::id() as i64),
            &weave_core::config::this_host(),
            &tags.repo,
            &tags.branch,
            &tags.worktree_id,
            &weave_core::config::Config::load().circle(),
            args.get("cert")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()),
            "",
        )
        .map_err(e)?;
    let tgt = if t.id.is_empty() { "-" } else { &t.id };
    let inj = if t.injectable() {
        "injectable"
    } else {
        "no-inject"
    };
    Ok(format!(
        "Attached '{me}' to the store [{}] {tgt} ({inj}). birth-cert: {cert}",
        t.mux.as_str()
    ))
}

/// `weave_spawn_peer` (WL-047): launch a NEW agent/command into a fresh mux pane (or
/// window when `window:true`) and thread an unguessable identity into its env so it
/// self-registers on its first `weave hook session`. Argv-only — the child command is
/// a JSON array of discrete argv strings, never a shell line.
///
/// SECURITY (remote surface): this is in `DANGEROUS_TOOLS` (blocked in safe HTTP
/// mode) AND the resolved `cwd` must be under a configured `spawn_allowed_dirs`
/// (`WEAVE_SPAWN_DIRS`) — an empty/unset allowlist DENIES every spawn here (a remote
/// caller must never pick an arbitrary cwd). The child PROGRAM (argv[0]) is
/// independently constrained to the injector's trusted dirs (see `inject::spawn`), so
/// two gates apply: program-trust AND cwd-allowlist.
///
/// Identity/cert: the parent pre-registers the new peer ONLY when the mux echoes a
/// usable target id (tmux/kitty/wezterm). In that case the STORE mints the
/// authoritative birth cert during pre-registration and we thread THAT cert into the
/// child env, so the child's later self-registration matches (a single authoritative
/// cert, no Store change). For muxes that do not echo an id (zellij/screen) we do not
/// pre-register; the child mints its own cert on self-registration.
///
/// All diagnostics go to stderr; the returned tool-result TEXT is the only stdout.
fn tool_spawn_peer(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    // The new child's weave identity (the peer row key). Bounded like any identity.
    let name = ident(args, "name", def)?;
    // Reject spawning over an already-registered peer (the spawn mints a fresh
    // identity; reusing a live one is almost always a mistake and would collide with
    // the existing peer's birth cert).
    if store.get_peer(&name).map_err(e)?.is_some() {
        return Err(format!(
            "a peer named '{name}' is already registered; choose a fresh name for the spawned agent."
        ));
    }
    // Child command: a JSON array of argv strings. Argv-only — never a shell line.
    let cmd_val = args
        .get("cmd")
        .ok_or("'cmd' is required (a JSON array of argv strings, e.g. [\"agent\", \"--flag\"]).")?;
    let arr = cmd_val
        .as_array()
        .ok_or("'cmd' must be an ARRAY of argv strings (argv-only; never a shell string).")?;
    if arr.is_empty() {
        return Err("'cmd' must not be empty (the first element is the program to run).".into());
    }
    if arr.len() > weave_inject::MAX_SPAWN_ARGS {
        return Err(format!(
            "'cmd' has too many arguments ({}; max {}).",
            arr.len(),
            weave_inject::MAX_SPAWN_ARGS
        ));
    }
    let mut argv_child: Vec<String> = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("'cmd[{i}]' must be a string (argv element)."))?;
        if !weave_inject::spawn_arg_ok(s) {
            return Err(format!(
                "'cmd[{i}]' is too long or contains control/NUL bytes (argv elements must be plain text)."
            ));
        }
        argv_child.push(s.to_string());
    }
    // Resolve the working directory (default: the server's cwd) and enforce the
    // allowlist HARD on this remote surface.
    let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(e)?,
    };
    let cfg = weave_core::config::Config::load();
    if !cfg.spawn_dir_allowed(std::path::Path::new(&cwd)) {
        return Err(format!(
            "refusing to spawn into {cwd:?}: not under a configured spawn_allowed_dirs \
             (set spawn_allowed_dirs / WEAVE_SPAWN_DIRS to permit this directory)."
        ));
    }
    // Resolve the target mux: an explicit override, else what the SERVER runs under.
    let mux = match args.get("mux").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => weave_inject::Mux::parse(s.trim()),
        _ => injector.detect_target().mux,
    };
    if matches!(mux, weave_inject::Mux::None) {
        return Err(
            "no multiplexer detected to spawn into (run inside tmux/zellij/kitty/wezterm/screen, \
             or pass an explicit \"mux\")."
                .into(),
        );
    }
    let window = args
        .get("window")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let circle = args
        .get("circle")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| cfg.circle());

    // Mint the authoritative birth cert in the PARENT (pure, no row) and thread it
    // into the child env. We do NOT pre-register a row before spawn — there is no
    // peer-delete to roll back a failed launch, so a pre-registered row could leak.
    // Instead we register AFTER a successful spawn that echoed a target id, binding
    // THIS cert on the fresh INSERT (register_peer_full honors a supplied cert for a
    // new peer). For muxes that echo no id (zellij/screen) we skip registration; the
    // child self-registers with the same env cert on its first hook.
    let cert = weave_core::store::mint_birth_cert().map_err(e)?;

    // Launch the child, threading WEAVE_SESSION / WEAVE_BIRTH_CERT / WEAVE_CIRCLE.
    let outcome = injector
        .spawn(mux, &cwd, &name, &cert, &circle, &argv_child, window)
        .map_err(e)?;
    // If the mux echoed a usable target id, register the peer now with the minted
    // cert so it is immediately injectable; the child's later self-registration with
    // the same env cert then matches (the UPDATE path preserves the stored cert).
    if !outcome.target.is_empty() && injector.id_valid(mux, &outcome.target) {
        store
            .register_peer_full(
                &name,
                mux.as_str(),
                &outcome.target,
                "",
                Some(cwd.as_str()),
                None,
                "",
                "",
                "",
                "",
                &circle,
                Some(cert.as_str()),
                "",
            )
            .map_err(e)?;
    }
    let tgt = if outcome.target.is_empty() {
        "(self-registers on first hook)".to_string()
    } else {
        outcome.target.clone()
    };
    Ok(format!(
        "Spawned '{name}' into {} {tgt} (cwd={cwd}). birth-cert: {cert}",
        mux.as_str()
    ))
}

/// `weave_kill_peer` (WL-047): terminate a registered peer's pane/session via the
/// per-mux kill argv. Looks up `(mux, target)` from the peer row, validates the
/// target with `id_valid`, then issues the kill. zellij/screen kills are COARSE
/// (session-level) by design — documented, never a precise per-pane guarantee.
/// In `DANGEROUS_TOOLS` (blocked in safe HTTP mode).
fn tool_kill_peer(
    store: &dyn Store,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'name' is required (the registered peer to kill).")?;
    let peer = store
        .get_peer(name)
        .map_err(e)?
        .ok_or_else(|| format!("no registered peer named '{name}'."))?;
    let target = Target::from_peer(&peer);
    if matches!(
        target.mux,
        weave_inject::Mux::ITerm2 | weave_inject::Mux::None
    ) {
        return Ok(format!(
            "peer '{name}' is on {} — kill is not supported for that backend (no clean argv).",
            target.mux.as_str()
        ));
    }
    // The target id is attacker-influenceable (captured from the peer's env at
    // register time); refuse to drive a mux with an id that doesn't match its shape.
    if !injector.id_valid(target.mux, &target.id) {
        return Err(format!(
            "refusing to kill: peer '{name}' has an invalid {} target {:?}.",
            target.mux.as_str(),
            target.id
        ));
    }
    match injector.kill(&target) {
        Ok(true) => Ok(format!(
            "Killed peer '{name}' on {} (target {}).",
            target.mux.as_str(),
            target.id
        )),
        // iTerm2/None are handled above, so a `false` here is a supported backend
        // whose kill command ran but reported failure: the pane/session is likely
        // already gone or the mux server is unreachable (e.g. a non-default tmux
        // socket). Report honestly instead of a false "killed".
        Ok(false) => Ok(format!(
            "could not confirm kill of '{name}' on {} (target {}) — the pane/session may already be gone or unreachable.",
            target.mux.as_str(),
            target.id
        )),
        Err(err) => Err(e(err)),
    }
}

/// `weave_set_description`: set the CALLER's OWN free-form task description (P5).
/// Self-only — the row key is the resolved caller identity (`ident`), never an
/// arg-supplied target (the `tool_attach` precedent). The store control-strips +
/// caps the text (oversized truncates, never errors); an empty string clears it.
/// All output is the returned tool-result TEXT (MCP stdout carries only JSON-RPC).
fn tool_set_description(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or("'description' is required (a one-line task summary; empty clears it).")?;
    store.set_description(&me, description).map_err(e)?;
    // Echo the stored (post-sanitize, post-TTL) view back so the caller sees what
    // actually persisted.
    let shown = store
        .get_peer(&me)
        .map_err(e)?
        .map(|p| p.description)
        .unwrap_or_default();
    if shown.is_empty() {
        Ok(format!("Cleared description for '{me}'."))
    } else {
        Ok(format!("Set description for '{me}': {shown}"))
    }
}

/// `weave_set_turn_state`: explicitly set the CALLER's OWN turn-state (P5).
/// Self-only (the `ident`-bound caller row). The store validates the state against
/// the `TurnState` enum — an unknown value is a hard error (the failure path).
fn tool_set_turn_state(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let state = args
        .get("state")
        .and_then(|v| v.as_str())
        .ok_or("'state' is required (pending_first_turn|working|awaiting_input|idle).")?;
    store.set_turn_state(&me, state).map_err(e)?;
    Ok(format!("Set turn_state for '{me}': {state}"))
}

fn tool_set_message_priority(store: &dyn Store, args: &Value) -> Result<String, String> {
    let message_id = args
        .get("message_id")
        .and_then(|v| v.as_i64())
        .ok_or("'message_id' is required.")?;
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .ok_or("'priority' is required (low, normal, high, urgent).")?;
    store
        .set_message_priority(message_id, priority)
        .map_err(e)?;
    Ok(format!(
        "Set priority of message #{message_id} to '{priority}'."
    ))
}

fn tool_set_peer_policy(store: &dyn Store, args: &Value) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("'name' is required.")?;
    let policy = args
        .get("policy")
        .and_then(|v| v.as_str())
        .ok_or("'policy' is required (open, auto, contacts_only, block_all).")?;
    let parsed = model::ContactPolicy::parse(policy);
    store.set_peer_policy(name, parsed.as_str()).map_err(e)?;
    Ok(format!(
        "Set contact_policy for '{name}': {}",
        parsed.as_str()
    ))
}

fn tool_get_peer_policy(store: &dyn Store, args: &Value) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("'name' is required.")?;
    match store.get_peer_policy(name).map_err(e)? {
        Some(p) => Ok(p),
        None => Err(format!("No peer '{name}' found.")),
    }
}

/// Connect handshake: capability-probe `peer` before sending. Reports a structured
/// verdict and degrades gracefully — a registered-but-not-alive or non-injectable
/// peer is NOT an error (`isError=false`); its messages still arrive via the store
/// on its next turn. Only a non-existent peer is an error.
fn tool_connect(
    store: &dyn Store,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the peer session name to connect to).")?;
    let to = bound_ident("to", to_raw)?;
    let Some(peer) = store.get_peer(&to).map_err(e)? else {
        // The only hard failure: the peer is not registered in this store.
        return Err(format!(
            "No registered peer '{to}'. Ask them to run `weave attach` (or share one WEAVE_DB)."
        ));
    };
    let target = Target::from_peer(&peer);
    let msg = match injector.capability(&target) {
        Capability::Live => format!(
            "Peer '{to}' is live [{}] {} — a live nudge can be delivered now.",
            target.mux.as_str(),
            target.id
        ),
        Capability::RegisteredNotAlive => format!(
            "Peer '{to}' is registered but not alive [{}] {} — delivery will be queued; \
             recipient drains on next turn.",
            target.mux.as_str(),
            target.id
        ),
        Capability::NotInjectable => format!(
            "Peer '{to}' is not injectable (mux=none) — delivery will be queued; \
             recipient drains on next turn."
        ),
    };
    Ok(msg)
}

fn mcp_peer_ambiguous_target_names(store: &dyn Store, to: &str) -> Vec<String> {
    let Ok(Some(peer)) = store.get_peer(to) else {
        return Vec::new();
    };
    if peer.mux.is_empty() || peer.mux == "none" || peer.target.is_empty() {
        return Vec::new();
    }
    let Ok(peers) = store.list_peers() else {
        return Vec::new();
    };
    let mut names = peers
        .into_iter()
        .filter(|p| p.mux == peer.mux && p.target == peer.target && p.socket == peer.socket)
        .map(|p| p.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    if names.len() > 1 {
        names
    } else {
        Vec::new()
    }
}

fn mcp_target_is_ambiguous(store: &dyn Store, to: &str) -> bool {
    !mcp_peer_ambiguous_target_names(store, to).is_empty()
}

/// Fire the caller-side live nudge for an ask/answer and compute the HONEST
/// delivery verdict, reusing the EXISTING injector return (no new spawn path, no
/// `store → inject` edge). This is the exact seam `tool_send` uses, lifted into a
/// helper so ask + answer surface the same normalized vocabulary:
///   * `inject_mode` returned `Ok(true)` (a nudge was actually injected) ⇒
///     `transport_delivered`;
///   * a registered-but-not-alive / `Ok(false)` / `Err` peer ⇒ `queued_next_turn`
///     (still succeeds; arrives on the recipient's next drain);
///   * `mux=none` / no peer row ⇒ `recipient_not_injectable`;
///   * shared mux target with another peer ⇒ `ambiguous_target_queued` and no live
///     injection attempt.
///
/// Advisory only: a queued / not-injectable / ambiguous delivery is NEVER an error.
fn ask_delivery_verdict(
    store: &dyn Store,
    nudge_template: Option<&str>,
    from: &str,
    to: &str,
    body: &str,
    injector: &dyn Injector,
) -> &'static str {
    let Ok(Some(peer)) = store.get_peer(to) else {
        return "recipient_not_injectable";
    };
    if mcp_target_is_ambiguous(store, to) {
        return "ambiguous_target_queued";
    }
    let target = Target::from_peer(&peer);
    match injector.capability(&target) {
        Capability::NotInjectable => "recipient_not_injectable",
        // Injectable (live or registered): fire the same paste-safe nudge tool_send
        // does and report whether it actually landed.
        _ => {
            let (nudge, mode) = build_nudge(nudge_template, from, body);
            match injector.inject_mode(&target, &nudge, mode) {
                Ok(true) => "transport_delivered",
                // Ok(false) (quiet/no-op) or Err (inject failed): the message is
                // safely in the store and arrives on the next drain.
                _ => "queued_next_turn",
            }
        }
    }
}

/// One-line human verdict sentence for an ask/answer result, from the normalized
/// verdict string.
fn verdict_sentence(verdict: &str, to: &str) -> String {
    match verdict {
        "transport_delivered" => format!("Live nudge delivered to '{to}' (transport_delivered)."),
        "queued_next_turn" => {
            format!("Queued for '{to}'; arrives on their next turn (queued_next_turn).")
        }
        "ambiguous_target_queued" => format!(
            "Live injection avoided for '{to}' because its mux target is ambiguous; arrives on their next drain (ambiguous_target_queued)."
        ),
        _ => format!(
            "'{to}' is not injectable; arrives on their next drain (recipient_not_injectable)."
        ),
    }
}

/// `weave_ask`: open a correlation-tracked request and return its id + the honest
/// delivery verdict. Point-to-point only (broadcast ask is P2). The live nudge is
/// fired caller-side here — no `store → inject` edge.
fn tool_ask(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the peer session name to ask).")?;
    let to = bound_ident("to", to_raw)?;
    if model::is_broadcast(&to) {
        return Err(
            "tracked ask is point-to-point; use weave_send for broadcast (broadcast ask is P2)."
                .to_string(),
        );
    }
    let to = store::resolve_point_recipient(store, &to).map_err(e)?;
    let no_memory = args
        .get("no_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body_raw = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required (the question).")?;
    let body = maybe_prefix_body_mcp(&from, body_raw, no_memory);
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let reply_to = args
        .get("reply_to")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(rt) = reply_to {
        if !model::ask_id_valid(rt) {
            return Err("'reply_to' is not a valid correlation id.".to_string());
        }
    }
    let kind = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(model::AskKind::parse)
        .unwrap_or_default();
    let options = args.get("options").and_then(|v| v.as_str());
    let (cid, qid) = store
        .ask(
            &from,
            &to,
            subject.as_deref(),
            &body,
            kind,
            options,
            reply_to,
        )
        .map_err(e)?;
    // P6: queued trace keyed by the QUESTION message id so `weave_delivery <qid>`
    // works uniformly. Best-effort; never sinks the ask.
    record_delivery_best_effort(
        store,
        qid,
        model::DeliveryRefKind::Ask,
        &to,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &to, &body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(store, qid, model::DeliveryRefKind::Ask, &to, stage, outcome);
    Ok(format!(
        "Opened ask {cid} from '{from}' to '{to}'. {}",
        verdict_sentence(verdict, &to)
    ))
}

fn routing_anomaly_body(ask_id: &str, expected: &str, actual: &str) -> String {
    format!("ROUTING_ANOMALY: ask for {expected} delivered to {actual} (ask {ask_id})")
}

fn report_routing_anomaly(
    store: &dyn Store,
    nudge_template: Option<&str>,
    injector: &dyn Injector,
    ask: &model::Ask,
    actual: &str,
) -> Result<Option<i64>, String> {
    if actual == ask.askee {
        return Ok(None);
    }
    let body = routing_anomaly_body(&ask.id, &ask.askee, actual);
    let mid = store
        .send(
            actual,
            &ask.asker,
            Some("ROUTING_ANOMALY"),
            &body,
            None,
            None,
        )
        .map_err(e)?;
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Message,
        &ask.asker,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, actual, &ask.asker, &body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Message,
        &ask.asker,
        stage,
        outcome,
    );
    Ok(Some(mid))
}

/// `weave_answer`: reply along a tracked thread back to the asker. Accepts either
/// `correlation_id` or an `in_reply_to` message id (resolved to its owning ask).
fn tool_answer(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let no_memory = args
        .get("no_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body_raw = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required (the answer).")?;
    let body = maybe_prefix_body_mcp(&from, body_raw, no_memory);
    let cid = resolve_correlation_id(store, args)?;
    let ask = store
        .get_ask(&cid)
        .map_err(e)?
        .ok_or_else(|| format!("No tracked ask '{cid}'."))?;
    let asker = ask.asker.clone();
    if from != ask.askee {
        let anomaly_mid = report_routing_anomaly(store, nudge_template, injector, &ask, &from)
            .ok()
            .flatten();
        let suffix = anomaly_mid
            .map(|mid| format!("; routing anomaly reported as #{mid}"))
            .unwrap_or_default();
        return Err(format!(
            "ROUTING_ANOMALY: ask for {} delivered to {} (ask {}){}",
            ask.askee, from, cid, suffix
        ));
    }
    let ans_id = store.answer(&from, &cid, &body).map_err(e)?;
    record_delivery_best_effort(
        store,
        ans_id,
        model::DeliveryRefKind::Answer,
        &asker,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &asker, &body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        ans_id,
        model::DeliveryRefKind::Answer,
        &asker,
        stage,
        outcome,
    );
    Ok(format!(
        "Answered ask {cid} back to '{asker}'. {}",
        verdict_sentence(verdict, &asker)
    ))
}

/// `weave_ack`: close a tracked thread (pure state transition). An optional
/// closing `message` is stored as the ask's close_note; P1 does NOT nudge on ack.
fn tool_ack(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let cid_raw = args
        .get("correlation_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'correlation_id' is required.")?;
    if !model::ask_id_valid(cid_raw) {
        return Err("'correlation_id' is not a valid correlation id.".to_string());
    }
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    store.ack(&from, cid_raw, message).map_err(e)?;
    let mut out = format!("Closed ask {cid_raw} (acked).");
    if message.is_some() {
        out.push_str(" Note recorded.");
    }
    // WL-036: fire `Ack` hooks post-state-change. The acker is the sender; the asker
    // (who learns of the ack) is the recipient. Best-effort lookup — a miss yields an
    // empty recipient (a `*` hook still fires). Failures log to stderr, never sink.
    let asker = store
        .get_ask(cid_raw)
        .ok()
        .flatten()
        .map(|a| a.asker)
        .unwrap_or_default();
    weave_inject::fire_post_send_hooks(
        &weave_core::config::Config::load(),
        weave_core::config::HookEvent::Ack,
        &from,
        &asker,
        cid_raw,
        0,
    );
    Ok(out)
}

/// `weave_asks`: list tracked asks where `me` plays `role` (asker/askee/any).
fn tool_asks(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let role = model::AskRole::parse(args.get("role").and_then(|v| v.as_str()).unwrap_or("any"));
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(200);
    let asks = store.list_asks(&me, role, limit).map_err(e)?;
    if asks.is_empty() {
        return Ok("No tracked asks.".to_string());
    }
    let mut out = format!("{} tracked ask(s):\n", asks.len());
    for a in &asks {
        let subj = a
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{} [{}] {} -> {}{} ({})\n",
            a.id,
            a.state.as_str(),
            a.asker,
            a.askee,
            subj,
            fmt_ts(a.opened_ts)
        ));
    }
    Ok(out)
}

/// `weave_ask_get`: fetch one tracked ask by correlation id.
fn tool_ask_get(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the correlation id).")?;
    if !model::ask_id_valid(id) {
        return Err("'id' is not a valid correlation id.".to_string());
    }
    let ask = store
        .get_ask(id)
        .map_err(e)?
        .ok_or_else(|| format!("No tracked ask '{id}'."))?;
    let answered = if ask.answer_msg_id.is_some() {
        " (answered)"
    } else {
        ""
    };
    Ok(format!(
        "{} [{}] {} -> {}{}{}",
        ask.id,
        ask.state.as_str(),
        ask.asker,
        ask.askee,
        ask.subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default(),
        answered
    ))
}

const AUTO_ACK_PREFIX: &str = "[weave-ack]";

#[derive(Debug, Clone)]
struct AskAutoAck {
    message_id: i64,
    status: &'static str,
    body: String,
    ts: i64,
}

fn parse_auto_ack_status(body: &str) -> Option<&'static str> {
    let rest = body.strip_prefix(AUTO_ACK_PREFIX)?.trim_start();
    let token = rest
        .split_whitespace()
        .next()
        .unwrap_or("received")
        .trim_end_matches(':');
    match token {
        "received" => Some("received"),
        "wrong-recipient" => Some("wrong-recipient"),
        "busy-queued" => Some("busy-queued"),
        "delegated-to-worker" => Some("delegated-to-worker"),
        "cannot-answer" => Some("cannot-answer"),
        "will-answer-later" => Some("will-answer-later"),
        _ => Some("received"),
    }
}

fn auto_ack_for_ask(store: &dyn Store, ask: &model::Ask) -> Option<AskAutoAck> {
    let thread = store.thread(ask.question_msg_id, 1_000).ok()?;
    thread
        .into_iter()
        .rev()
        .filter(|m| {
            m.sender == ask.askee
                && m.recipient == ask.asker
                && m.in_reply_to == Some(ask.question_msg_id)
        })
        .filter_map(|m| {
            parse_auto_ack_status(&m.body).map(|status| AskAutoAck {
                message_id: m.id,
                status,
                body: m.body,
                ts: m.ts,
            })
        })
        .next()
}

fn ask_status_token(
    ask: &model::Ask,
    question_trace: &[model::DeliveryTrace],
    question_receipts: &[(String, i64)],
    auto_ack: Option<&AskAutoAck>,
) -> &'static str {
    match ask.state {
        model::AskState::Acked => "acked",
        model::AskState::Answered => "answered",
        model::AskState::Open => {
            if let Some(ack) = auto_ack {
                ack.status
            } else if !question_receipts.is_empty()
                || question_trace
                    .iter()
                    .any(|t| t.stage == model::DeliveryStage::Drained.as_str())
            {
                "received"
            } else if question_trace.iter().any(|t| {
                t.stage == model::DeliveryStage::Injected.as_str()
                    && t.outcome == model::DeliveryOutcome::Ok.as_str()
            }) {
                "injected"
            } else if question_trace.iter().any(|t| {
                t.stage == model::DeliveryStage::Queued.as_str()
                    || t.stage == model::DeliveryStage::NotInjectable.as_str()
            }) {
                "queued"
            } else {
                "opened"
            }
        }
    }
}

fn responder_status_body(status: &str) -> Result<(&'static str, &'static str), String> {
    match status {
        "received" => Ok(("received", "ask received; queued for this session")),
        "wrong-recipient" => Ok(("wrong-recipient", "ask appears to be routed to the wrong session")),
        "busy-queued" => Ok(("busy-queued", "session is busy; ask is queued for a later answer")),
        "delegated-to-worker" => Ok(("delegated-to-worker", "ask was delegated to a background worker")),
        "cannot-answer" => Ok(("cannot-answer", "session cannot answer this ask")),
        "will-answer-later" => Ok(("will-answer-later", "session will answer later")),
        _ => Err(format!(
            "invalid responder status '{status}' (expected received|wrong-recipient|busy-queued|delegated-to-worker|cannot-answer|will-answer-later)"
        )),
    }
}

fn responder_health(store: &dyn Store, me: &str) -> Result<(usize, usize), String> {
    let asks = store.list_asks(me, model::AskRole::Askee, 200).map_err(e)?;
    let mut open = 0usize;
    let mut unacknowledged = 0usize;
    for ask in asks
        .into_iter()
        .filter(|a| a.state == model::AskState::Open)
    {
        open += 1;
        if auto_ack_for_ask(store, &ask).is_none() {
            unacknowledged += 1;
        }
    }
    Ok((open, unacknowledged))
}

fn tool_responder(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    if args
        .get("health")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let (open, unacknowledged) = responder_health(store, &me)?;
        return serde_json::to_string_pretty(&json!({
            "me": me,
            "running": false,
            "open": open,
            "unacknowledged": unacknowledged,
        }))
        .map_err(e);
    }
    let status_arg = args
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("received");
    let (status, text) = responder_status_body(status_arg)?;
    let asks = store
        .list_asks(&me, model::AskRole::Askee, 200)
        .map_err(e)?;
    let mut rows = Vec::new();
    for ask in asks
        .into_iter()
        .filter(|a| a.state == model::AskState::Open)
    {
        if auto_ack_for_ask(store, &ask).is_some() {
            continue;
        }
        let body = format!("{AUTO_ACK_PREFIX} {status}: {text}");
        let mid = store.reply(&me, ask.question_msg_id, &body).map_err(e)?;
        record_delivery_best_effort(
            store,
            mid,
            model::DeliveryRefKind::Message,
            &ask.asker,
            model::DeliveryStage::Queued,
            model::DeliveryOutcome::Ok,
        );
        let verdict = ask_delivery_verdict(store, nudge_template, &me, &ask.asker, &body, injector);
        let (stage, outcome) = verdict_to_stage(verdict);
        record_delivery_best_effort(
            store,
            mid,
            model::DeliveryRefKind::Message,
            &ask.asker,
            stage,
            outcome,
        );
        rows.push(json!({
            "id": ask.id,
            "asker": ask.asker,
            "askee": ask.askee,
            "ack_message_id": mid,
            "ack_status": status,
            "verdict": verdict,
        }));
    }
    serde_json::to_string_pretty(&json!({
        "me": me,
        "acknowledged": rows.len(),
        "asks": rows,
    }))
    .map_err(e)
}

/// `weave_ask_status`: show read-time delivery/response status for a tracked ask.
fn tool_ask_status(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the correlation id).")?;
    if !model::ask_id_valid(id) {
        return Err("'id' is not a valid correlation id.".to_string());
    }
    let ask = store
        .get_ask(id)
        .map_err(e)?
        .ok_or_else(|| format!("No tracked ask '{id}'."))?;
    let question_delivery = store
        .list_delivery(ask.question_msg_id, model::MAX_DELIVERY_ROWS)
        .map_err(e)?;
    let question_receipts = store.receipts(ask.question_msg_id).map_err(e)?;
    let answer_delivery = match ask.answer_msg_id {
        Some(mid) => store
            .list_delivery(mid, model::MAX_DELIVERY_ROWS)
            .map_err(e)?,
        None => Vec::new(),
    };
    let answer_receipts = match ask.answer_msg_id {
        Some(mid) => store.receipts(mid).map_err(e)?,
        None => Vec::new(),
    };
    let auto_ack = auto_ack_for_ask(store, &ask);
    let routing_status = ask_status_token(
        &ask,
        &question_delivery,
        &question_receipts,
        auto_ack.as_ref(),
    );
    let mut out = format!(
        "Ask {} [{}] {} -> {} status={routing_status}\n",
        ask.id,
        ask.state.as_str(),
        ask.asker,
        ask.askee
    );
    out.push_str(&format!(
        "Question #{}: {} delivery stage(s), {} receipt(s)\n",
        ask.question_msg_id,
        question_delivery.len(),
        question_receipts.len()
    ));
    if let Some(ack) = &auto_ack {
        out.push_str(&format!(
            "Auto-ACK #{}: {} at {}\n",
            ack.message_id,
            ack.status,
            fmt_ts(ack.ts)
        ));
        out.push_str(&format!("Auto-ACK body: {}\n", ack.body));
    }
    if let Some(mid) = ask.answer_msg_id {
        out.push_str(&format!(
            "Answer #{mid}: {} delivery stage(s), {} receipt(s)\n",
            answer_delivery.len(),
            answer_receipts.len()
        ));
    }
    Ok(out)
}

/// `weave_ask_many`: fan ONE question to N explicit peers. Opens a parent group +
/// one normal P1 child ask per (de-duped, valid, non-broadcast) peer, then fires the
/// per-child live nudge CALLER-SIDE for each created child (the `ask_delivery_verdict`
/// seam — NO `store → inject` edge). Best-effort: an invalid/broadcast peer in the
/// list carries a per-child error and the call still succeeds; an empty / over-cap
/// list is a hard whole-call error. Non-blocking — returns the parent id + per-child
/// correlation ids/verdicts immediately (no quorum, no retry).
fn tool_ask_many(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    // `to` must be a non-empty JSON array of strings.
    let to_arr = args
        .get("to")
        .and_then(|v| v.as_array())
        .ok_or("'to' is required (a JSON array of peer session names).")?;
    if to_arr.is_empty() {
        return Err("'to' must list at least one peer.".to_string());
    }
    if to_arr.len() > store::MAX_ASK_MANY_TARGETS {
        return Err(format!(
            "'to' lists {} peers; max {} per ask-many.",
            to_arr.len(),
            store::MAX_ASK_MANY_TARGETS
        ));
    }
    // Bound each entry to a trimmed identity (over-length / non-string is a hard
    // whole-call error; a metachar-but-valid-length id is left to the per-child
    // best-effort path in the store). A broadcast entry stays in the list and is
    // rejected PER-CHILD by the store (best-effort), matching repowire.
    let mut peers: Vec<String> = Vec::with_capacity(to_arr.len());
    for (i, v) in to_arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("'to[{i}]' must be a string."))?;
        peers.push(bound_ident("to", s)?);
    }
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required (the question).")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let outcome = store
        .create_ask_many(&from, &peers, subject.as_deref(), body)
        .map_err(e)?;
    // Per-child caller-side nudge for each CREATED child (no store→inject edge).
    let mut created = 0usize;
    let mut failed = 0usize;
    let mut lines = String::new();
    for (peer, res) in &outcome.children {
        match res {
            Ok(cid) => {
                created += 1;
                let verdict =
                    ask_delivery_verdict(store, nudge_template, &from, peer, body, injector);
                lines.push_str(&format!("  {peer}: {cid} — {verdict}\n"));
            }
            Err(err) => {
                failed += 1;
                lines.push_str(&format!("  {peer}: FAILED — {err}\n"));
            }
        }
    }
    Ok(format!(
        "Opened ask-many {} from '{from}' to {} peer(s): {created} created, {failed} failed.\n{lines}",
        outcome.parent_id,
        outcome.children.len()
    ))
}

/// `weave_ask_many_result`: aggregate an ask-many group at READ time. Renders the
/// rollup counts, the derived state (`complete|partial|pending`), the pending peer
/// list, and per-child state/answer ids. Read-only, no nudge. An unknown/invalid
/// parent id is a clean error.
fn tool_ask_many_result(store: &dyn Store, args: &Value) -> Result<String, String> {
    let parent_id = args
        .get("parent_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'parent_id' is required (the ask-many id).")?;
    if !model::ask_many_id_valid(parent_id) {
        return Err("'parent_id' is not a valid ask-many id.".to_string());
    }
    let age = args.get("age").and_then(|v| v.as_i64());
    let r = store
        .ask_many_result(parent_id, age)
        .map_err(e)?
        .ok_or_else(|| format!("No ask-many '{parent_id}'."))?;
    let mut out = format!(
        "ask-many {} [{}] from '{}' — {}/{} answered, {} acked, {} pending, {} failed (opened {}).\n",
        r.parent_id,
        r.state.as_str(),
        r.asker,
        r.answered,
        r.target_count,
        r.acked,
        r.pending,
        r.failed,
        fmt_ts(r.opened_ts)
    );
    let mut pending_peers: Vec<&str> = Vec::new();
    for c in &r.children {
        let state = c.state.map(|s| s.as_str()).unwrap_or("failed");
        if c.state == Some(model::AskState::Open) {
            pending_peers.push(&c.peer);
        }
        let cid = c.correlation_id.as_deref().unwrap_or("-");
        let ans = c
            .answer_msg_id
            .map(|m| format!(" answer=#{m}"))
            .unwrap_or_default();
        out.push_str(&format!("  {} [{state}] {cid}{ans}\n", c.peer));
    }
    if !pending_peers.is_empty() {
        out.push_str(&format!("pending: {}\n", pending_peers.join(", ")));
    }
    Ok(out)
}

/// Resolve the correlation id `weave_answer` targets: prefer an explicit
/// `correlation_id`, else map an `in_reply_to` message id to its owning ask via a
/// `get_ask`-free lookup walking `list_asks` is too coarse, so we resolve through
/// the store's question/answer message ids. A reference resolving to no ask is a
/// clean error.
fn resolve_correlation_id(store: &dyn Store, args: &Value) -> Result<String, String> {
    if let Some(cid) = args
        .get("correlation_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !model::ask_id_valid(cid) {
            return Err("'correlation_id' is not a valid correlation id.".to_string());
        }
        return Ok(cid.to_string());
    }
    if let Some(mid) = args.get("in_reply_to").and_then(|v| v.as_i64()) {
        // Map a message id to its owning ask (`WHERE question_msg_id=? OR
        // answer_msg_id=?`). A reference resolving to no ask is a clean error.
        let cid = store
            .ask_for_message(mid)
            .map_err(e)?
            .ok_or_else(|| format!("message #{mid} does not belong to any tracked ask."))?;
        return Ok(cid);
    }
    Err("provide either 'correlation_id' or 'in_reply_to'.".to_string())
}

// ---- P3 job board (poll-only) -------------------------------------------
// Seven tools mirroring repowire 1:1 under weave's `weave_` prefix. NO injector
// involvement — jobs do not nudge in P3. All caps + id validation + attempt_id
// fencing + state-machine enforcement live in the STORE, so these tools inherit
// them; the failure paths (stale_attempt / unknown job / illegal transition /
// oversized JSON / bad id) surface as JSON-RPC errors via `map_err(e)`.

/// Render a [`model::Job`] as a one-line human summary for the tools.
fn job_line(j: &model::Job) -> String {
    let assignee = j.assignee.as_deref().unwrap_or("-");
    format!(
        "{} [{}] {} (creator={}, assignee={}, updated {})",
        j.id,
        j.state.as_str(),
        j.title,
        j.creator,
        assignee,
        fmt_ts(j.updated_ts)
    )
}

/// `weave_job_create`: mint a new `queued` board job. creator = me. Returns the
/// minted job_id + state. No nudge.
fn tool_job_create(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let creator = ident(args, "creator", def)?;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'title' is required.")?;
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let spec = model::JobSpec {
        title: title.to_string(),
        description: str_arg("description"),
        kind: str_arg("kind"),
        owner: str_arg("owner"),
        assignee: str_arg("assignee"),
        circle: str_arg("circle"),
        prompt: str_arg("prompt"),
        correlation_id: str_arg("correlation_id"),
        source_kind: str_arg("source_kind"),
        source_id: str_arg("source_id"),
        scope: str_arg("scope"),
        visibility: str_arg("visibility"),
        deadline_at: args.get("deadline_at").and_then(|v| v.as_i64()),
        expires_at: args.get("expires_at").and_then(|v| v.as_i64()),
    };
    let job = store.create_job(&creator, spec).map_err(e)?;
    Ok(format!(
        "Created job {} [{}] '{}' (creator={}).",
        job.id,
        job.state.as_str(),
        job.title,
        job.creator
    ))
}

/// `weave_job_delegate`: orchestration-first worker handoff. Creates a queued job
/// assigned to one worker and sends that worker a durable `JOB_DELEGATED` message.
fn tool_job_delegate(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let creator = ident(args, "from", def)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the worker peer alias or sess_<16-hex>).")?;
    let to_bound = bound_ident("to", to_raw)?;
    if model::is_broadcast(&to_bound) {
        return Err("job delegation is point-to-point; choose one worker peer.".to_string());
    }
    let to = store::resolve_point_recipient(store, &to_bound).map_err(e)?;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'title' is required.")?;
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let spec = model::JobSpec {
        title: title.to_string(),
        description: str_arg("description"),
        kind: str_arg("kind"),
        owner: Some(creator.clone()),
        assignee: Some(to.clone()),
        circle: str_arg("circle"),
        prompt: str_arg("prompt"),
        deadline_at: args.get("deadline_at").and_then(|v| v.as_i64()),
        ..Default::default()
    };
    let job = store.create_job(&creator, spec).map_err(e)?;
    let body = format!(
        "JOB_DELEGATED {}\nfrom: {}\nassignee: {}\ntitle: {}\n\n{}",
        job.id,
        creator,
        to,
        job.title,
        job.prompt
            .as_deref()
            .or_else(|| (!job.description.is_empty()).then_some(job.description.as_str()))
            .unwrap_or("Claim or inspect this job with weave job show/status/result.")
    );
    let trace_id = model::mint_trace_id();
    let mid = store
        .send(
            &creator,
            &to,
            Some(&format!("Job: {}", job.title)),
            &body,
            None,
            Some(&trace_id),
        )
        .map_err(e)?;
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Message,
        &to,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, &creator, &to, &body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Message,
        &to,
        stage,
        outcome,
    );
    Ok(format!(
        "Delegated job {} [{}] '{}' creator={} assignee={} delegation_message_id={} verdict={}.",
        job.id,
        job.state.as_str(),
        job.title,
        job.creator,
        to,
        mid,
        verdict
    ))
}

/// `weave_job_list`: list board jobs filtered by state/owner/creator/assignee/circle.
/// Read-only, bounded by clamp_limit in the store.
fn tool_job_list(store: &dyn Store, args: &Value) -> Result<String, String> {
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let state = match args.get("state").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Some(model::JobState::from_str(s.trim())?),
        _ => None,
    };
    let filter = model::JobFilter {
        state,
        owner: str_arg("owner"),
        creator: str_arg("creator"),
        assignee: str_arg("assignee"),
        circle: str_arg("circle"),
    };
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(200);
    let jobs = store.list_jobs(filter, limit).map_err(e)?;
    if jobs.is_empty() {
        return Ok("No jobs.".to_string());
    }
    let mut out = format!("{} job(s):\n", jobs.len());
    for j in &jobs {
        out.push_str(&job_line(j));
        out.push('\n');
    }
    Ok(out)
}

/// `weave_job_show` / `weave_job_status`: fetch one job by id (the canonical detail
/// view; the two names are aliases, repowire parity). Unknown id is a clean error.
fn tool_job_status(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'job_id' is required.")?;
    if !model::job_id_valid(id) {
        return Err("'job_id' is not a valid job id.".to_string());
    }
    let job = store
        .get_job(id)
        .map_err(e)?
        .ok_or_else(|| format!("No job '{id}'."))?;
    let mut out = job_line(&job);
    if let Some(ref p) = job.phase {
        out.push_str(&format!("\nphase: {p}"));
    }
    if let Some(ref n) = job.progress_note {
        out.push_str(&format!("\nnote: {n}"));
    }
    if job.cancel_requested {
        out.push_str("\ncancel_requested: true");
    }
    Ok(out)
}

/// `weave_job_claim`: CLAIM a job — mint an attempt_id, set assignee, transition to
/// running. Returns the attempt_id (the worker captures it to fence its updates). A
/// terminal job cannot be claimed.
fn tool_job_claim(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'job_id' is required.")?;
    if !model::job_id_valid(id) {
        return Err("'job_id' is not a valid job id.".to_string());
    }
    let assignee = ident(args, "assignee", def)?;
    let job = store
        .claim_job(id, &assignee)
        .map_err(e)?
        .ok_or_else(|| format!("No job '{id}'."))?;
    let attempt = job.attempt_id.as_deref().unwrap_or("-");
    Ok(format!(
        "Claimed job {} as '{}'; attempt_id={} state={}.",
        job.id,
        assignee,
        attempt,
        job.state.as_str()
    ))
}

/// `weave_job_update`: apply a lifecycle/result patch, fenced by attempt_id and the
/// state machine (BOTH enforced in the store). Failure paths surface as JSON-RPC
/// errors: stale attempt → "stale_attempt"; unknown job → not found; illegal
/// transition → error; oversized JSON → cap error.
fn tool_job_update(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'job_id' is required.")?;
    if !model::job_id_valid(id) {
        return Err("'job_id' is not a valid job id.".to_string());
    }
    let attempt_id = args
        .get("attempt_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let state = match args.get("state").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Some(model::JobState::from_str(s.trim())?),
        _ => None,
    };
    // result/error/artifacts accept either a JSON object/array or a JSON string; we
    // store the serialized text either way (the store size-caps it).
    let json_arg = |k: &str| -> Option<String> {
        args.get(k).and_then(|v| match v {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
    };
    let patch = model::JobPatch {
        state,
        state_reason: str_arg("state_reason"),
        phase: str_arg("phase"),
        progress_note: str_arg("progress_note"),
        result_summary: str_arg("result_summary"),
        result_json: json_arg("result"),
        error_json: json_arg("error"),
        artifacts_json: json_arg("artifacts"),
    };
    let job = store.update_job(id, attempt_id, patch).map_err(e)?;
    Ok(format!("Updated job {} [{}].", job.id, job.state.as_str()))
}

/// `weave_job_result`: the read-time result view. Terminal → payload; else a
/// not_ready marker. Read-only.
fn tool_job_result(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'job_id' is required.")?;
    if !model::job_id_valid(id) {
        return Err("'job_id' is not a valid job id.".to_string());
    }
    let r = store
        .job_result(id)
        .map_err(e)?
        .ok_or_else(|| format!("No job '{id}'."))?;
    if !r.ready {
        return Ok(format!(
            "job {} [{}] not_ready (no terminal result yet).",
            r.id,
            r.state.as_str()
        ));
    }
    let summary = r.result_summary.as_deref().unwrap_or("-");
    Ok(format!(
        "job {} [{}] summary={} result={} error={} artifacts={}",
        r.id,
        r.state.as_str(),
        summary,
        r.result_json,
        r.error_json,
        r.artifacts_json
    ))
}

/// `weave_job_cancel`: COOPERATIVE cancel (never a hard delete) — no confirm gate.
/// requested_by = me. A queued job terminal-cancels; a claimed/running job just gets
/// the cancel_requested flag (worker honors it on its next poll).
fn tool_job_cancel(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'job_id' is required.")?;
    if !model::job_id_valid(id) {
        return Err("'job_id' is not a valid job id.".to_string());
    }
    let me = ident(args, "from", def)?;
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let job = store
        .cancel_job(id, &me, reason)
        .map_err(e)?
        .ok_or_else(|| format!("No job '{id}'."))?;
    Ok(format!(
        "Cancel requested for job {} (state {}, cancel_requested={}).",
        job.id,
        job.state.as_str(),
        job.cancel_requested
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

/// Canonical catalog of every `weave_*` operation (name, description, inputSchema).
/// The single source of truth for the meta-tool's `describe`/`search`/`list` modes
/// and for eager-flat mode. Every operation in `call_tool` has exactly one entry here.
fn tool_catalog() -> Vec<Value> {
    #[allow(unused_mut)]
    let mut list = json!([
        {
            "name": "weave_send",
            "description": "Send a message to another agent session. 'to' = a session name, or 'all'/'*' to broadcast. If the recipient is a registered injectable peer (tmux/zellij), a live nudge is pushed into its pane immediately; otherwise it arrives on the recipient's next turn. Cross-store (Tier-2): pass 'to_store' = a path to another store to queue the message as an intent in YOUR OWN outbox; the recipient pulls and commits it on its next drain (no foreign write, no broadcast).",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name, or 'all'."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "to_store":{"type":"string","description":"Cross-store: path to the recipient's store. Queues a directed intent in your outbox (next-drain delivery); not valid with broadcast."},
                "to_host":{"type":"string","description":"Optional host hint for a cross-store intent (advisory)."},
                "no_memory":{"type":"boolean","description":"Skip memory context prefixing."},
                "idempotencyKey":{"type":"string","description":"Optional idempotency key. A duplicate key returns the existing message id instead of creating a new row."},
                "priority":{"type":"string","description":"Message priority: low, normal, high, urgent (default normal)."},
                "supersedes":{"type":"integer","description":"Optional id of a prior message of YOURS this one replaces; the predecessor is marked superseded and hidden from the recipient's unread inbox (kept, flagged, in history). You may only supersede your own messages."},
                "ttl":{"type":"integer","description":"Optional ephemeral TTL in seconds (1..=86400). The message is auto-deleted (delete-on-sweep) after this many seconds and excluded from every read surface; omit for a permanent message."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_push",
            "description": "Cross-machine PUSH RECEIVE handler (WL-056 / ADR-0005): accept a signed Intent delivered over the bearer-gated `weave serve --write` POST /api surface and commit it into THIS machine's inbox via the SAME Tier-2 pull-commit pipeline (re-validate, verify signature, Store::send assigns id/ts locally), then light this pane without polling. This is the RECEIVE side (no host) — the A-initiated dual of a Tier-2 pull. To SEND a push to another machine, use the `weave push --to <name> --host <url:port>` CLI verb. Owner-only-writes: the recipient commits its own row; dedup is by idempotency_key.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"The sender's session name."},
                "to":{"type":"string","description":"Recipient session name on THIS machine (directed-only; no broadcast)."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "sig":{"type":"string","description":"Optional ed25519 signature over the canonical (from,to,body); verified under this receiver's trust policy on commit."},
                "to_host":{"type":"string","description":"Optional host hint (advisory)."},
                "idempotency_key":{"type":"string","description":"Idempotency key; a retried POST with the same key never double-commits. Synthesized from (from,body) if omitted."},
                "trace_id":{"type":"string","description":"Optional end-to-end trace id."},
                "priority":{"type":"string","description":"Message priority: low, normal, high, urgent (default normal)."},
                "ttl":{"type":"integer","description":"Optional ephemeral TTL in seconds (1..=86400)."}
            },"required":["from","to","body"]}
        },
        {
            "name": "weave_notify",
            "description": "Fire-and-forget notification to a peer (no reply expected). Persists the message and pushes a live nudge if the recipient is injectable, then returns the HONEST delivery verdict: transport_delivered (nudge landed live) / queued_next_turn (registered or not alive — arrives on next drain) / recipient_not_injectable / ambiguous_target_queued. An unknown peer is NOT an error — the message still waits in the store. Point-to-point only; use weave_send for broadcast.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name (point-to-point; broadcast is not supported)."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "idempotencyKey":{"type":"string","description":"Optional idempotency key. A duplicate key returns the existing message id instead of creating a new row."},
                "priority":{"type":"string","description":"Message priority: low, normal, high, urgent (default normal)."},
                "ttl":{"type":"integer","description":"Optional ephemeral TTL in seconds (1..=86400); the message is auto-deleted after this many seconds."},
                "dedupIdle":{"type":"boolean","description":"Idle-notification dedup: mark this as an idle 'still waiting' ping and auto-supersede YOUR prior UNREAD idle pings to this recipient so they collapse to just the latest. Never touches a real message or another sender's pings (default false)."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_broadcast_notify",
            "description": "Broadcast a fire-and-forget notification to all online peers in your circle. Fan-out: one message per online peer, plus a live nudge for each injectable peer. Returns an aggregated delivery verdict per peer. Offline peers are skipped.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "circle":{"type":"string","description":"Scope to this circle; omit for your own configured circle."},
                "priority":{"type":"string","description":"Message priority: low, normal, high, urgent (default normal)."}
            },"required":["body"]}
        },
        {
            "name": "weave_broadcast_ask",
            "description": "Broadcast a tracked ask to all online peers in your circle. Fan-out via ask-many: one tracked question per online peer. Returns a parent id and per-child delivery verdicts. Offline peers are skipped.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "circle":{"type":"string","description":"Scope to this circle; omit for your own configured circle."},
                "reply_to":{"type":"integer","description":"Optional message id this broadcast ask replies to."}
            },"required":["body"]}
        },
        {
            "name": "weave_delivery",
            "description": "Show the DELIVERY/transport trace for a message (queued -> injected/inject_failed/not_injectable -> drained). The transport-side complement to weave_receipts (which shows READ receipts). Read-only and metadata-only — the trace never carries the message body. An unknown/never-traced id returns an empty trace, not an error.",
            "inputSchema": {"type":"object","properties":{
                "message_id":{"type":"integer","description":"The message id to show the delivery trace for."},
                "limit":{"type":"integer","description":"Max stages to return (bounded by the server)."}
            },"required":["message_id"]}
        },
        {
            "name": "weave_outbox",
            "description": "List pending cross-store intents in your outbox (Tier-2). Read-only self-inspection of messages you queued for recipients in other stores that have not yet been pulled.",
            "inputSchema": {"type":"object","properties":{
                "limit":{"type":"integer"}
            },"required":[]}
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
            "name": "weave_search",
            "description": "Full-text search over messages (FTS5 on sqlite, LIKE fallback on libsql). Returns matching messages newest-first.",
            "inputSchema": {"type":"object","properties":{
                "query":{"type":"string","description":"Search query string (FTS5 syntax on sqlite, substring on libsql)."},
                "limit":{"type":"integer"}
            },"required":["query"]}
        },
        {
            "name": "weave_sessions",
            "description": "List session names seen, with unread counts and last activity. By default scoped to your own circle; pass circle='*' for mesh-wide (an orchestrator caller defaults to mesh-wide).",
            "inputSchema": {"type":"object","properties":{
                "circle":{"type":"string","description":"Scope to this circle; '*' = every circle (mesh-wide). Omit for your own circle."}
            },"required":[]}
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
            "description": "List registered peers and whether each is injectable (live push) or delivery-on-next-turn. By default scoped to your own circle; pass circle='*' for mesh-wide (an orchestrator caller defaults to mesh-wide).",
            "inputSchema": {"type":"object","properties":{
                "circle":{"type":"string","description":"Scope to this circle; '*' = every circle (mesh-wide). Omit for your own circle."}
            },"required":[]}
        },
        {
            "name": "weave_scan",
            "description": "Scan, identify, and tag running sessions: refresh YOUR OWN row's git tags (repo/branch/worktree), then list every (federated) peer with liveness and its repo/branch/worktree tags. Optional 'repo'/'branch' filters narrow the set by exact tag match. Only ever (re)registers the caller's own row (owner-only-writes); other/federated rows are read-only.",
            "inputSchema": {"type":"object","properties":{
                "repo":{"type":"string","description":"Only show peers whose repo tag equals this value."},
                "branch":{"type":"string","description":"Only show peers whose branch tag equals this value."},
                "circle":{"type":"string","description":"Scope to this circle; '*' = every circle (mesh-wide). Omit for your own circle."}
            },"required":[]}
        },
        {
            "name": "weave_reply",
            "description": "Reply to a message by id. The recipient is derived from the parent message (the reply is addressed back to the parent's other party), so you only give your name, the parent id, and the body. Like weave_send, a live nudge is pushed if the recipient is an injectable peer.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "in_reply_to":{"type":"integer","description":"The message id you're replying to."},
                "body":{"type":"string"},
                "no_memory":{"type":"boolean","description":"Skip memory context prefixing."},
                "ttl":{"type":"integer","description":"Optional ephemeral TTL in seconds (1..=86400); the reply is auto-deleted after this many seconds."}
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
            "description": "Echo the resolved identity (default session from WEAVE_SESSION), the active storage backend, your visibility circle, your orchestrator role, and how this process would inject. Use to confirm who you are before sending.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_attach",
            "description": "Adopt this session into the shared store WITHOUT restarting: re-capture the current pane env and upsert YOUR OWN peer row. Makes you visible/injectable to other sessions immediately. Only ever (re)registers the caller's own identity.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":[]}
        },
        {
            "name": "weave_spawn_peer",
            "description": "Launch a NEW agent/command into a fresh mux pane (or a new window with window:true) and thread an unguessable identity into its environment so it self-registers on its first weave hook. ARGV-ONLY: cmd is an array of argv strings, never a shell line — no shell is ever invoked. The child program (cmd[0]) must resolve to a trusted directory, and on the MCP/remote surface the cwd must be under a configured spawn_allowed_dirs (WEAVE_SPAWN_DIRS) — an unset allowlist DENIES the spawn. Returns the minted identity and birth-cert. Dangerous (disabled in safe HTTP mode).",
            "inputSchema": {"type":"object","properties":{
                "name":{"type":"string","description":"The new spawned agent's session identity (the peer row key). Must not already exist."},
                "cmd":{"type":"array","items":{"type":"string"},"description":"The child command as an ARRAY of argv strings, e.g. [\"agent\",\"--flag\"]. Argv-only; never a shell string."},
                "cwd":{"type":"string","description":"Working directory to launch in (default: the server's cwd). Must be under spawn_allowed_dirs on this surface."},
                "mux":{"type":"string","description":"Override the multiplexer (tmux|zellij|kitty|wezterm|screen). Default: whatever the server runs under."},
                "window":{"type":"boolean","description":"Open a new window/tab instead of a split pane (default false)."},
                "circle":{"type":"string","description":"Visibility circle for the child (default: the server's circle)."}
            },"required":["name","cmd"]}
        },
        {
            "name": "weave_kill_peer",
            "description": "Terminate a registered peer's pane/session via the per-mux kill argv (tmux kill-pane, wezterm kill-pane, kitty close-window; zellij/screen are COARSE session-level kills). iterm2/none are unsupported. Dangerous (disabled in safe HTTP mode).",
            "inputSchema": {"type":"object","properties":{
                "name":{"type":"string","description":"The registered peer to kill."}
            },"required":["name"]}
        },
        {
            "name": "weave_set_description",
            "description": "Set YOUR OWN free-form, self-reported task description (a one-line summary). Surfaces compactly in weave_peers/weave_sessions/weave_scan and to whoami, and ages out after the description TTL (900s). Self-only: only ever updates the caller's own peer row. Oversized text is truncated (never an error); control chars are stripped. Pass an empty string to clear it.",
            "inputSchema": {"type":"object","properties":{
                "description":{"type":"string","description":"One-line task summary (capped + control-stripped; empty clears it)."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["description"]}
        },
        {
            "name": "weave_set_turn_state",
            "description": "Explicitly set YOUR OWN turn-state (P5 rich presence). Normally hook-auto via `weave hook session|prompt|stop|wake`; this is the manual override. Self-only. An invalid state is an error.",
            "inputSchema": {"type":"object","properties":{
                "state":{"type":"string","enum":["pending_first_turn","working","awaiting_input","idle"],"description":"The turn-state to set."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["state"]}
        },
        {
            "name": "weave_set_message_priority",
            "description": "Set the priority of an existing message. low, normal, high, urgent (default normal).",
            "inputSchema": {"type":"object","properties":{
                "message_id":{"type":"integer","description":"The message id to update."},
                "priority":{"type":"string","description":"Priority level: low, normal, high, urgent."}
            },"required":["message_id","priority"]}
        },
        {
            "name": "weave_set_peer_policy",
            "description": "Set a peer's contact policy. open (default), auto, contacts_only, block_all.",
            "inputSchema": {"type":"object","properties":{
                "name":{"type":"string","description":"The peer session name."},
                "policy":{"type":"string","description":"Policy: open, auto, contacts_only, block_all."}
            },"required":["name","policy"]}
        },
        {
            "name": "weave_get_peer_policy",
            "description": "Get a peer's current contact policy. Returns open, auto, contacts_only, block_all, or an error if the peer is not found.",
            "inputSchema": {"type":"object","properties":{
                "name":{"type":"string","description":"The peer session name."}
            },"required":["name"]}
        },
        {
            "name": "weave_connect",
            "description": "Probe whether a peer can be reached by a live nudge right now, and report the verdict (live / registered-but-not-alive / not-injectable). A not-alive or non-injectable peer is NOT an error — its messages are still delivered via the store on its next turn; only a non-existent peer is an error.",
            "inputSchema": {"type":"object","properties":{
                "to":{"type":"string","description":"The peer session name to connect to."}
            },"required":["to"]}
        },
        {
            "name": "weave_ask",
            "description": "Open a correlation-TRACKED request to a peer and return its correlation_id immediately (NON-blocking — not a synchronous RPC). The question is delivered like a normal message (live nudge if injectable, else next-turn) and the result reports the honest delivery verdict (transport_delivered / queued_next_turn / recipient_not_injectable / ambiguous_target_queued). Point-to-point only (no broadcast). Optional reply_to chains a new ask off a prior one, closing the prior thread.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"The peer session name to ask."},
                "body":{"type":"string","description":"The question."},
                "subject":{"type":"string"},
                "reply_to":{"type":"string","description":"Optional prior correlation_id this ask chains/closes."},
                "no_memory":{"type":"boolean","description":"Skip memory context prefixing."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_answer",
            "description": "Answer a tracked ask, replying back along the thread to whoever opened it and transitioning the ask open->answered. Reference the thread by correlation_id OR by an in_reply_to message id (resolved to its owning ask). Reports the honest delivery verdict to the asker. Errors on an unknown thread, an already-acked thread, or a responder who is not the askee.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (must be the askee)."},
                "correlation_id":{"type":"string","description":"The ask's correlation_id."},
                "in_reply_to":{"type":"integer","description":"Alternatively, a message id belonging to the ask."},
                "body":{"type":"string","description":"The answer."},
                "no_memory":{"type":"boolean","description":"Skip memory context prefixing."}
            },"required":["body"]}
        },
        {
            "name": "weave_ack",
            "description": "Close a tracked ask (transition -> acked). A pure state transition; an optional 'message' is recorded as the closing note (NOT delivered/nudged — send a weave_answer first if you need it delivered). Errors on an unknown thread, a double-ack, or an acker who is not the askee.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (must be the askee)."},
                "correlation_id":{"type":"string","description":"The ask's correlation_id."},
                "message":{"type":"string","description":"Optional closing note (stored, not delivered)."}
            },"required":["correlation_id"]}
        },
        {
            "name": "weave_asks",
            "description": "List tracked asks where you are the asker, the askee, or either (role: asker|askee|any, default any). Read-only.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string"},
                "role":{"type":"string","description":"asker | askee | any (default any)."},
                "limit":{"type":"integer"}
            },"required":[]}
        },
        {
            "name": "weave_ask_get",
            "description": "Fetch a single tracked ask by correlation_id (its state, parties, subject, and whether it has been answered). Read-only.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"The correlation_id."}
            },"required":["id"]}
        },
        {
            "name": "weave_ask_status",
            "description": "Show read-time delivery/response status for a tracked ask: ask state, routing_status (opened|queued|injected|received|answered|acked), delivery stage counts, read receipt counts, and any non-closing [weave-ack] auto-ACK. Read-only.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"The correlation_id."}
            },"required":["id"]}
        },
        {
            "name": "weave_responder",
            "description": "Run one non-disruptive responder sweep for YOUR session: send one idempotent [weave-ack] status reply for each open ask addressed to you, without marking the question read and without answering/closing the ask. Returns JSON with acknowledged count and per-ask ack ids.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "status":{"type":"string","enum":["received","wrong-recipient","busy-queued","delegated-to-worker","cannot-answer","will-answer-later"],"description":"ACK status token (default received)."},
                "health":{"type":"boolean","description":"Report open/unacknowledged ask counts without sending ACKs."}
            },"required":[]}
        },
        {
            "name": "weave_ask_many",
            "description": "Fan ONE question to N explicit peers in parallel and return a parent_id + per-child correlation_ids immediately (NON-blocking, best-effort — no quorum, no retry). Opens a parent ask-many group and one normal tracked child ask per peer; each child is delivered like a normal message (live nudge if injectable, else next-turn) and reports its honest delivery verdict. An invalid/broadcast peer in the list is a per-child error, NOT a whole-call failure; an empty or over-cap list IS an error. Children answer/ack exactly like any weave_ask. Use weave_ask_many_result to aggregate. Explicit peer LIST only (no broadcast/circle).",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"array","items":{"type":"string"},"description":"The peer session names to ask (1..=64, de-duplicated)."},
                "body":{"type":"string","description":"The question (delivered to every target)."},
                "subject":{"type":"string"}
            },"required":["to","body"]}
        },
        {
            "name": "weave_ask_many_result",
            "description": "Aggregate an ask-many group at READ time (no background ticker): the per-child state/answer, the pending peer list, the rollup counts, and the derived state (complete = no child pending; partial = some pending AND past the optional age threshold; pending otherwise). Read-only. Errors on an unknown or invalid parent_id.",
            "inputSchema": {"type":"object","properties":{
                "parent_id":{"type":"string","description":"The ask-many parent id."},
                "age":{"type":"integer","description":"Optional age (seconds): a still-pending group older than this reads as 'partial' (daemon-free, opt-in)."}
            },"required":["parent_id"]}
        },
        {
            "name": "weave_job_create",
            "description": "Create a durable board job in the 'queued' state and return its server-minted job_id (P3 poll-only work queue — NO autonomous dispatch/runner; nothing nudges or spawns). The creator is you; owner defaults to the creator. A worker later CLAIMs the job to work it. Title required; other fields are inert board metadata (kind/circle/prompt/visibility/deadline_at/expires_at as epoch seconds, etc.).",
            "inputSchema": {"type":"object","properties":{
                "creator":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "title":{"type":"string","description":"Short job title (required)."},
                "description":{"type":"string","description":"The work request / details."},
                "kind":{"type":"string","description":"Job kind label (default 'general')."},
                "owner":{"type":"string","description":"Owning peer (default: creator)."},
                "assignee":{"type":"string","description":"Pre-assigned worker (optional)."},
                "circle":{"type":"string","description":"Board circle/scope label (optional)."},
                "prompt":{"type":"string","description":"Board metadata prompt (NOT runner exec config)."},
                "deadline_at":{"type":"integer","description":"Optional deadline (epoch seconds)."},
                "expires_at":{"type":"integer","description":"Optional expiry (epoch seconds)."},
                "visibility":{"type":"string","description":"Visibility label (default 'circle')."}
            },"required":["title"]}
        },
        {
            "name": "weave_job_delegate",
            "description": "Create a queued board job assigned to one worker AND send that worker a durable JOB_DELEGATED message/nudge. `to` accepts either a peer alias or stable sess_<16-hex> id from peers/scan/sessions. Weave coordinates and records; workers still claim/update/result through the normal job tools.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Orchestrator/creator session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Worker peer alias or sess_<16-hex> session id."},
                "title":{"type":"string","description":"Short job title (required)."},
                "description":{"type":"string","description":"The work request / details."},
                "kind":{"type":"string","description":"Job kind label (default 'general')."},
                "circle":{"type":"string","description":"Board circle/scope label (optional)."},
                "prompt":{"type":"string","description":"Prompt text sent in the JOB_DELEGATED message."},
                "deadline_at":{"type":"integer","description":"Optional deadline (epoch seconds)."}
            },"required":["to","title"]}
        },
        {
            "name": "weave_job_list",
            "description": "List board jobs, filtered by any of state/owner/creator/assignee/circle (exact match; omit to leave unconstrained), newest-first by update time and bounded. Read-only.",
            "inputSchema": {"type":"object","properties":{
                "state":{"type":"string","description":"queued|dispatching|delivered|running|awaiting_input|completed|failed|cancelled|blocked|expired|unavailable"},
                "owner":{"type":"string"},
                "creator":{"type":"string"},
                "assignee":{"type":"string"},
                "circle":{"type":"string"},
                "limit":{"type":"integer"}
            },"required":[]}
        },
        {
            "name": "weave_job_show",
            "description": "Show a single board job's full status by job_id (state, parties, phase, latest note, cancel flag). Read-only. Alias of weave_job_status.",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_job_status",
            "description": "Show a single board job's status by job_id (the canonical detail view; alias of weave_job_show). Read-only.",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_job_claim",
            "description": "CLAIM a board job to work it: mints a fresh attempt_id (claim token), sets you as the assignee, and transitions the job to 'running'. Returns the attempt_id — capture it and pass it to weave_job_update so your updates are FENCED (a re-claim by another worker mints a new token and invalidates yours). A terminal job cannot be claimed.",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id to claim."},
                "assignee":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_job_update",
            "description": "Update a board job's lifecycle/result. If the job is CLAIMED you MUST pass the matching attempt_id or the update is rejected ('stale_attempt') — this fences out a stale worker. The state transition is validated (an illegal one errors). progress_note is appended to an append-only event log; entering a terminal state (completed/failed/cancelled/expired/unavailable) stamps completion. result/error/artifacts accept a JSON object/array (size-capped).",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id."},
                "attempt_id":{"type":"string","description":"Your claim token (required to update a claimed job)."},
                "state":{"type":"string","description":"New lifecycle state (validated)."},
                "state_reason":{"type":"string"},
                "phase":{"type":"string","description":"Free-form worker phase label."},
                "progress_note":{"type":"string","description":"Appended to the progress log."},
                "result_summary":{"type":"string"},
                "result":{"description":"Terminal result payload (JSON object/array or string)."},
                "error":{"description":"Error payload (JSON object/array or string)."},
                "artifacts":{"description":"Artifacts payload (JSON array or string)."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_job_result",
            "description": "Read a board job's result. A terminal job returns its summary/result/error/artifacts payload; a non-terminal job returns a 'not_ready' marker. Read-only.",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_job_cancel",
            "description": "COOPERATIVELY cancel a board job (never a hard delete — no confirm needed). A 'queued' job transitions straight to 'cancelled'; a claimed/running job only gets a cancel_requested flag that its worker honors on its next poll (the daemon-free contract); a terminal job just records the request. Cancel requested-by is you.",
            "inputSchema": {"type":"object","properties":{
                "job_id":{"type":"string","description":"The job id."},
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "reason":{"type":"string","description":"Optional cancellation reason."}
            },"required":["job_id"]}
        },
        {
            "name": "weave_claim_orchestrator",
            "description": "Claim the single per-circle orchestrator role for yourself. Refused if a DIFFERENT live orchestrator already holds the circle, unless force=true steals it (a non-destructive role-bit flip — the demoted peer can re-claim; no data is lost, so no confirm is required). Demotes any prior orchestrators in the circle to 'peer' in one transaction. An unregistered caller is an error.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "circle":{"type":"string","description":"Circle to claim (defaults to your own circle)."},
                "force":{"type":"boolean","description":"Steal the role even from a live orchestrator."}
            },"required":[]}
        },
        {
            "name": "weave_orchestrator_status",
            "description": "Report the live orchestrator of a circle (or that none is present). 'live' reuses weave's daemon-free liveness verdict (no new probe).",
            "inputSchema": {"type":"object","properties":{
                "circle":{"type":"string","description":"Circle to query (defaults to your own circle)."}
            },"required":[]}
        },
        {
            "name": "weave_daemon_start",
            "description": "Start the optional presence daemon in the background. Idempotent: if the daemon is already running, returns the existing PID. The daemon writes periodic heartbeats to the presence table so peers show live status; when stopped, the system degrades transparently to the TTL heuristic.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":[]}
        },
        {
            "name": "weave_daemon_stop",
            "description": "Stop the optional presence daemon. Sends SIGTERM to the recorded PID and cleans up the pidfile. Safe to call even if the daemon is not running.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_daemon_status",
            "description": "Show whether the optional presence daemon is running. Returns running:true + pid when active, or running:false when stopped. A stale pidfile is automatically cleaned up.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_schedule",
            "description": "Schedule a future message delivery (one-shot or recurring). Provide exactly one of 'at' (absolute UNIX timestamp) or 'every' (cron preset like @hourly/@daily/@weekly/@monthly or a 5-field cron expression). The scheduled message is sent to the recipient's inbox and behaves like a normal message on delivery.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name."},
                "subject":{"type":"string"},
                "body":{"type":"string","description":"Message body (required)."},
                "at":{"type":"integer","description":"One-shot: absolute UNIX timestamp."},
                "every":{"type":"string","description":"Recurring: cron preset (@hourly, @daily, @weekly, @monthly) or 5-field cron expression."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_schedules",
            "description": "List your scheduled messages (one-shot and recurring). Includes cancelled and executed rows so you see the full state.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "limit":{"type":"integer"}
            },"required":[]}
        },
        {
            "name": "weave_cancel_schedule",
            "description": "Soft-cancel a scheduled message by its id. Idempotent: cancelling an already-cancelled or executed row is a no-op, not an error.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"integer","description":"The schedule id to cancel."}
            },"required":["id"]}
        },
        {
            "name": "weave_tick",
            "description": "Execute any due scheduled messages now (explicit tick). Self-only by default: only schedules you created are fired. Pass all=true to fire every due schedule (admin/debug).",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "all":{"type":"boolean","description":"Fire schedules for all senders, not just yourself."}
            },"required":[]}
        },
        {
            "name": "weave_memory_write",
            "description": "Write a memory entry to a scope. Scopes: global, project (derived from cwd), persona (derived from identity), orchestrator (derived from circle). Optional 'name' overrides derivation.",
            "inputSchema": {"type":"object","properties":{
                "scope":{"type":"string","enum":["global","project","persona","orchestrator"],"description":"Scope kind."},
                "name":{"type":"string","description":"Optional explicit scope name (overrides derivation)."},
                "key":{"type":"string","description":"Entry key (alphanumeric, hyphen, underscore; max 128)."},
                "title":{"type":"string","description":"Entry title (max 256 chars)."},
                "tags":{"type":"array","items":{"type":"string"},"description":"Optional tags (max 16, each max 64 chars)."},
                "body":{"type":"string","description":"Entry body (max 64KiB)."}
            },"required":["scope","key","title","body"]}
        },
        {
            "name": "weave_memory_read",
            "description": "Read a memory entry by scope and key.",
            "inputSchema": {"type":"object","properties":{
                "scope":{"type":"string","enum":["global","project","persona","orchestrator"]},
                "name":{"type":"string","description":"Optional explicit scope name."},
                "key":{"type":"string","description":"Entry key."}
            },"required":["scope","key"]}
        },
        {
            "name": "weave_memory_search",
            "description": "Search memory entries. Simple substring search over tags, title, and body. Omit scope to search across all scopes with entries.",
            "inputSchema": {"type":"object","properties":{
                "scope":{"type":"string","enum":["global","project","persona","orchestrator"]},
                "name":{"type":"string","description":"Optional explicit scope name."},
                "query":{"type":"string","description":"Search substring."},
                "limit":{"type":"integer","description":"Max results (bounded by server)."}
            },"required":["query"]}
        },
        {
            "name": "weave_memory_list",
            "description": "List all memory entries in a scope.",
            "inputSchema": {"type":"object","properties":{
                "scope":{"type":"string","enum":["global","project","persona","orchestrator"]},
                "name":{"type":"string","description":"Optional explicit scope name."}
            },"required":["scope"]}
        },
        {
            "name": "weave_memory_delete",
            "description": "Delete a memory entry by scope and key.",
            "inputSchema": {"type":"object","properties":{
                "scope":{"type":"string","enum":["global","project","persona","orchestrator"]},
                "name":{"type":"string","description":"Optional explicit scope name."},
                "key":{"type":"string","description":"Entry key."}
            },"required":["scope","key"]}
        },
        {
            "name": "weave_review_queue",
            "description": "List PR review items. Filter by all, open, pending (unreviewed), or reviewed.",
            "inputSchema": {"type":"object","properties":{
                "filter":{"type":"string","enum":["all","open","pending","reviewed"],"description":"Filter by review state."},
                "limit":{"type":"integer","description":"Max results (bounded by server)."}
            },"required":[]}
        },
        {
            "name": "weave_review_add",
            "description": "Add a GitHub PR to the review queue.",
            "inputSchema": {"type":"object","properties":{
                "pr_url":{"type":"string","description":"GitHub pull request URL."},
                "title":{"type":"string","description":"PR title."},
                "author":{"type":"string","description":"PR author."},
                "repo":{"type":"string","description":"Repository name (owner/repo)."}
            },"required":["pr_url"]}
        },
        {
            "name": "weave_review_mark",
            "description": "Mark a review item as reviewed.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"Review item id."},
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["id"]}
        },
        {
            "name": "weave_review_remove",
            "description": "Remove a review item from the queue.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"Review item id."}
            },"required":["id"]}
        },
        {
            "name": "weave_ask_permission",
            "description": "Request approval for a mutating tool (Bash, Edit, Write) from a peer. Creates a ToolPermission ask. The peer answers with 'approve' or 'deny'. Unanswered asks timeout after 300s and are treated as denied.",
            "inputSchema": {"type":"object","properties":{
                "to":{"type":"string","description":"Peer session name to ask for approval."},
                "tool":{"type":"string","description":"Tool name (e.g. Bash, Edit, Write)."},
                "args":{"type":"string","description":"Tool arguments / command."},
                "body":{"type":"string","description":"Optional explanatory message."},
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["to","tool"]}
        },
        {
            "name": "weave_permission_status",
            "description": "Check the permission status of a ToolPermission ask. Returns pending, approved, denied, or timeout.",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"Permission ask correlation id."},
                "timeout":{"type":"integer","description":"Custom timeout in seconds (default 300)."}
            },"required":["id"]}
        },
        {
            "name": "weave_permission_list",
            "description": "List ToolPermission asks you created, with their current verdict.",
            "inputSchema": {"type":"object","properties":{
                "limit":{"type":"integer","description":"Max results (bounded by server)."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":[]}
        },
        {
            "name": "weave_lease_reserve",
            "description": "Reserve an advisory lease on a resource. Succeeds only if no active lease exists (or it has expired).",
            "inputSchema": {"type":"object","properties":{
                "resource":{"type":"string","description":"Resource identifier (path, glob, or freeform tag)."},
                "ttl":{"type":"integer","description":"TTL in seconds (1..86400)."},
                "note":{"type":"string","description":"Optional note."}
            },"required":["resource","ttl"]}
        },
        {
            "name": "weave_lease_release",
            "description": "Release a lease you hold on a resource.",
            "inputSchema": {"type":"object","properties":{
                "resource":{"type":"string","description":"Resource identifier."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["resource"]}
        },
        {
            "name": "weave_lease_list",
            "description": "List active (non-expired) leases.",
            "inputSchema": {"type":"object","properties":{
                "limit":{"type":"integer","description":"Max results (bounded by server)."}
            },"required":[]}
        },
        {
            "name": "weave_lease_sweep",
            "description": "Remove all expired leases and return the count swept.",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        },
        {
            "name": "weave_thread_summarize",
            "description": "Generate or retrieve a cached LLM summary for a message thread. If a cached summary exists and refresh is not requested, it is returned immediately.",
            "inputSchema": {"type":"object","properties":{
                "root_id":{"type":"integer","description":"The message id at the root of the thread."},
                "refresh":{"type":"boolean","description":"Force a fresh summary even if a cached one exists."}
            },"required":["root_id"]}
        },
        {
            "name": "weave_summarize_text",
            "description": "Summarize arbitrary text via the configured LLM endpoint. Does not persist the summary.",
            "inputSchema": {"type":"object","properties":{
                "text":{"type":"string","description":"The text to summarize."}
            },"required":["text"]}
        }
    ]);
    // WL-049 / ADR-0002: ONE token-light governed web-access dispatcher (proxies all
    // 35 obscura browser_* ops; per-op schemas fetched on demand via describe). Only
    // present in an `--features obscura` build, so the default tool table is unchanged.
    #[cfg(feature = "obscura")]
    if let Some(arr) = list.as_array_mut() {
        arr.push(json!({
            "name": "weave_web",
            "description": "Governed stealth web access via obscura (deny-by-default). ONE dispatcher proxying all browser_* ops behind weave's permission/lease/job gate. 'action' = the op (e.g. 'navigate','snapshot','click','extract'); 'args' = that op's arguments (e.g. {\"url\":\"https://…\"}). action='list' enumerates the ops; describe=true returns an op's forwarding note without running it. URL-bearing ops are SSRF-guarded (internal/localhost/private hosts denied by default). Optional 'lease_ttl' rate-limits per host; 'audit'=true records a durable job.",
            "inputSchema": {"type":"object","properties":{
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "action":{"type":"string","description":"The browser op to run (e.g. 'navigate'); or 'list' to enumerate ops."},
                "args":{"type":"object","description":"Arguments for the op, forwarded opaquely to obscura (e.g. {\"url\":\"https://example.com\"})."},
                "describe":{"type":"boolean","description":"Return the op's forwarding note instead of running it (progressive disclosure)."},
                "lease_ttl":{"type":"integer","description":"Optional: reserve a per-host lease for this many seconds (rate / mutual-exclusion)."},
                "audit":{"type":"boolean","description":"Optional: record a durable job row auditing this web op."}
            },"required":["action"]}
        }));
    }
    list.as_array().cloned().unwrap_or_default()
}

/// Whether the MCP server exposes the full **eager-flat** tool table (every
/// `weave_*` op as a standing tool) instead of the token-light progressive-disclosure
/// surface. Off by default (WL-050 / ADR-0003). Set `WEAVE_MCP_EAGER=1` (or `true`)
/// for harnesses that require flat tools — no capability or compatibility is lost.
fn eager_mode() -> bool {
    std::env::var("WEAVE_MCP_EAGER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The token-light `weave` meta-tool (WL-050 / ADR-0003). It exposes the FULL
/// operation set on demand — `search` to find ops, `describe` to fetch one op's
/// schema, `call` to invoke it, `list` to enumerate — so per-op schemas are not
/// paid as standing context. One standing tool replaces 70+ flat tools.
fn meta_tool_def() -> Value {
    json!({
        "name": "weave",
        "description": "Token-light gateway to every weave operation (messaging, asks, peers, jobs, leases, orchestration, review, schedules, memory, permissions, daemon, web). The full operation set is reachable WITHOUT loading every schema into context: mode='search' {query} finds ops; mode='list' enumerates them; mode='describe' {name} returns one op's argument schema; mode='call' {name, arguments} runs it. Op names may omit the 'weave_' prefix (e.g. 'send' == 'weave_send'). For flat tools instead, start the server with WEAVE_MCP_EAGER=1.",
        "inputSchema": {"type":"object","properties":{
            "mode":{"type":"string","enum":["search","describe","call","list"],"description":"search: find ops by keyword; list: all op names; describe: one op's schema; call: invoke an op."},
            "query":{"type":"string","description":"For mode=search: keyword matched against op name + description (case-insensitive). Empty = all."},
            "name":{"type":"string","description":"For mode=describe/call: the operation name, e.g. 'weave_send' or 'send'."},
            "arguments":{"type":"object","description":"For mode=call: the called operation's own arguments object."},
            "limit":{"type":"integer","description":"For mode=search: max matches to return (default 40, capped)."}
        },"required":["mode"]}
    })
}

/// The STANDING MCP surface returned by `tools/list`.
///
/// - **Progressive disclosure (default):** just the `weave` meta-tool (~a few hundred
///   tokens). The full operation set is reachable via `search`/`describe`/`call`, so
///   the standing context cost stays bounded regardless of how many ops exist
///   (the `token-light` invariant, ADR-0003).
/// - **Eager-flat (`WEAVE_MCP_EAGER=1`):** the complete catalog, byte-identical to the
///   pre-WL-050 table, for harnesses that require flat tools.
fn tools() -> Value {
    if eager_mode() {
        Value::Array(tool_catalog())
    } else {
        json!([meta_tool_def()])
    }
}

/// First sentence (through the first ". ") of a tool description — a compact summary
/// for `mode=search` results so the listing itself stays token-light.
fn first_sentence(desc: &str) -> String {
    match desc.split_once(". ") {
        Some((head, _)) => format!("{head}."),
        None => desc.to_string(),
    }
}

/// Accept an operation name with or without the `weave_` prefix: `send` -> `weave_send`,
/// `weave_send` -> `weave_send`. The bare `weave` meta-tool name is returned unchanged so
/// `mode=call` can reject self-targeting.
fn normalize_op_name(name: &str) -> String {
    let n = name.trim();
    if n == "weave" || n.starts_with("weave_") {
        n.to_string()
    } else {
        format!("weave_{n}")
    }
}

/// WL-050 / ADR-0003: the `weave` meta-tool. Progressive disclosure over the full
/// operation catalog so the standing MCP surface stays token-light:
/// - `search {query, limit?}` — find ops by keyword (name + description), compact summaries.
/// - `list` — enumerate every operation name.
/// - `describe {name}` — return one op's full `{name, description, inputSchema}`.
/// - `call {name, arguments}` — invoke an op via `call_tool`, preserving every guard.
///
/// `call` re-applies the safe-HTTP destructive-op gate to the *inner* op (so the meta-tool
/// is not a bypass) and refuses to target the meta-tool itself (no recursion).
#[allow(clippy::too_many_arguments)]
fn tool_meta(
    store: &dyn Store,
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    args: &Value,
    injector: &dyn Injector,
    dangerous: bool,
) -> Result<String, String> {
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    match mode {
        "search" => {
            let q = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(40)
                .clamp(1, 200) as usize;
            let mut matches: Vec<Value> = Vec::new();
            for t in tool_catalog() {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                if q.is_empty()
                    || name.to_lowercase().contains(&q)
                    || desc.to_lowercase().contains(&q)
                {
                    matches.push(json!({"name": name, "summary": first_sentence(desc)}));
                    if matches.len() >= limit {
                        break;
                    }
                }
            }
            Ok(json!({"count": matches.len(), "matches": matches}).to_string())
        }
        "list" => {
            let names: Vec<String> = tool_catalog()
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            Ok(json!({"count": names.len(), "operations": names}).to_string())
        }
        "describe" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                return Err("mode=describe requires 'name' (the operation to describe)".into());
            }
            let want = normalize_op_name(name);
            match tool_catalog()
                .into_iter()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(want.as_str()))
            {
                Some(def) => Ok(def.to_string()),
                None => Err(format!(
                    "unknown operation '{name}' (try mode=search or mode=list)"
                )),
            }
        }
        "call" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                return Err("mode=call requires 'name' (the operation to invoke)".into());
            }
            let want = normalize_op_name(name);
            if want == "weave" {
                return Err("mode=call cannot target the 'weave' meta-tool itself".into());
            }
            // Preserve the safe-HTTP destructive-op gate on the INNER op — the meta-tool
            // must never be a way around it (parity with the flat dispatch path).
            if !dangerous && is_dangerous_tool(&want) {
                return Err(format!(
                    "Tool '{want}' is disabled in safe HTTP mode. Start with --dangerous to enable."
                ));
            }
            let inner_args = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            call_tool(
                store,
                me_default,
                nudge_template,
                extra_dbs,
                pull,
                &want,
                &inner_args,
                injector,
                dangerous,
            )
        }
        "" => Err("missing 'mode' (one of: search, describe, call, list)".into()),
        other => Err(format!(
            "unknown mode '{other}' (one of: search, describe, call, list)"
        )),
    }
}

fn tool_daemon_start(me_default: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", me_default)?;
    let pidfile = daemon_pidfile();
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if pidfile.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if daemon_running(pid) {
                    return Ok(format!(r#"{{"started":false,"pid":{pid}}}"#));
                }
            }
        }
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("could not resolve current executable: {e}"))?;
    let child = std::process::Command::new(&exe)
        .args(["daemon", "run", "--me", &me])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;
    let pid = child.id();
    if let Err(e) = std::fs::write(&pidfile, pid.to_string()) {
        return Err(format!("daemon spawned but pidfile write failed: {e}"));
    }
    Ok(format!(r#"{{"started":true,"pid":{pid}}}"#))
}

fn tool_daemon_stop() -> Result<String, String> {
    let pidfile = daemon_pidfile();
    if !pidfile.exists() {
        return Ok(r#"{"stopped":false}"#.to_string());
    }
    let pid_str =
        std::fs::read_to_string(&pidfile).map_err(|e| format!("could not read pidfile: {e}"))?;
    let pid = pid_str
        .trim()
        .parse::<u32>()
        .map_err(|_| "pidfile contains invalid pid".to_string())?;
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let _ = std::fs::remove_file(&pidfile);
    Ok(r#"{"stopped":true}"#.to_string())
}

fn tool_daemon_status() -> Result<String, String> {
    let pidfile = daemon_pidfile();
    if !pidfile.exists() {
        return Ok(r#"{"running":false}"#.to_string());
    }
    let pid_str = std::fs::read_to_string(&pidfile).unwrap_or_default();
    let pid = pid_str.trim().parse::<u32>().unwrap_or(0);
    if daemon_running(pid) {
        Ok(format!(r#"{{"running":true,"pid":{pid}}}"#))
    } else {
        let _ = std::fs::remove_file(&pidfile);
        Ok(r#"{"running":false}"#.to_string())
    }
}

fn tool_schedule(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (recipient session name).")?;
    let to = bound_ident("to", to_raw)?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;
    let at = args.get("at").and_then(|v| v.as_i64());
    let every = args.get("every").and_then(|v| v.as_str());

    let (kind, cron_expr, next_run) = match (at, every) {
        (Some(ts), None) => {
            if ts <= 0 {
                return Err("'at' must be a positive UNIX timestamp".to_string());
            }
            (model::ScheduleKind::OneShot, String::new(), ts)
        }
        (None, Some(expr)) => {
            let expr = expr.trim();
            if !model::cron_valid(expr) {
                return Err("'every' is not a valid cron expression".to_string());
            }
            let next = model::next_occurrence(expr, model::now()).ok_or_else(|| {
                "could not compute next occurrence from cron expression".to_string()
            })?;
            (model::ScheduleKind::Recurring, expr.to_string(), next)
        }
        (Some(_), Some(_)) => {
            return Err("provide exactly one of 'at' or 'every', not both".to_string());
        }
        (None, None) => {
            return Err("provide exactly one of 'at' or 'every'".to_string());
        }
    };

    let id = store
        .schedule_message(
            &from,
            &to,
            subject.as_deref(),
            body,
            kind,
            &cron_expr,
            next_run,
        )
        .map_err(e)?;
    Ok(format!(
        "Scheduled message #{id}: {from} -> {to} at {next_run} ({})",
        kind.as_str()
    ))
}

fn tool_schedules(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let rows = store.list_schedules(&me, limit).map_err(e)?;
    if rows.is_empty() {
        return Ok(format!("No scheduled messages for '{me}'."));
    }
    let mut out = format!("Scheduled messages for '{me}':\n");
    for s in &rows {
        let subj = s
            .subject
            .as_ref()
            .map(|s| format!(" | {s}"))
            .unwrap_or_default();
        let state = if s.cancelled {
            "cancelled"
        } else if s.executed_ts.is_some() {
            "executed"
        } else {
            "pending"
        };
        out.push_str(&format!(
            "#{} [{}] {} -> {}{} ({}) next={}\n",
            s.id,
            state,
            s.sender,
            s.recipient,
            subj,
            s.kind.as_str(),
            s.next_run
        ));
    }
    Ok(out)
}

fn tool_cancel_schedule(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or("'id' is required (the schedule id to cancel).")?;
    let cancelled = store.cancel_schedule(id).map_err(e)?;
    if cancelled {
        Ok(format!("Cancelled schedule #{id}."))
    } else {
        Ok(format!(
            "Schedule #{id} was already terminal or did not exist."
        ))
    }
}

fn tool_tick(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let now_ts = model::now();
    let due = store.get_due_schedules(now_ts).map_err(e)?;
    let mut fired = 0usize;
    let mut skipped = 0usize;
    for sched in &due {
        if !all && sched.sender != me {
            skipped += 1;
            continue;
        }
        let _mid = store
            .send(
                &sched.sender,
                &sched.recipient,
                sched.subject.as_deref(),
                &sched.body,
                None,
                None,
            )
            .map_err(e)?;
        store.mark_schedule_executed(sched.id).map_err(e)?;
        fired += 1;
    }
    Ok(format!(
        "Tick: {fired} schedule(s) fired, {skipped} skipped."
    ))
}

// ---------------------------------------------------------------------------
// Memory helpers (WL-017)
// ---------------------------------------------------------------------------

/// Optionally prepend memory context to a body. Non-fatal: any problem returns the original body.
fn maybe_prefix_body_mcp(identity: &str, body: &str, no_memory: bool) -> String {
    if no_memory {
        return body.to_string();
    }
    let circle = weave_core::config::Config::load().circle();
    let prefix = memory::build_context_prefix(identity, &circle, body, 3);
    if prefix.is_empty() {
        body.to_string()
    } else {
        format!("{prefix}{body}")
    }
}

fn parse_memory_scope_mcp(
    scope: &str,
    name: Option<&str>,
    identity: &str,
) -> Result<memory::MemoryScope, String> {
    match scope {
        "global" => Ok(memory::MemoryScope::Global),
        "project" => {
            if let Some(n) = name {
                Ok(memory::MemoryScope::Project(n.to_string()))
            } else {
                memory::project_scope_from_cwd().ok_or_else(|| {
                    "not in a git repo; specify 'name' or run inside a git repo".to_string()
                })
            }
        }
        "persona" => {
            let id = name.unwrap_or(identity);
            Ok(memory::MemoryScope::Persona(id.to_string()))
        }
        "orchestrator" => {
            let circle = name
                .map(|s| s.to_string())
                .unwrap_or_else(|| weave_core::config::Config::load().circle());
            Ok(memory::MemoryScope::Orchestrator(circle))
        }
        other => Err(format!(
            "unknown scope '{other}'; must be global, project, persona, or orchestrator"
        )),
    }
}

fn tool_memory_write(def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let scope_raw = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or("'scope' is required (global, project, persona, orchestrator).")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let scope = parse_memory_scope_mcp(scope_raw, name, &me)?;
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or("'key' is required.")?;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or("'title' is required.")?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or("'body' is required.")?;
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    memory::memory_write(&scope, key, title, &tags, body).map_err(|e| e.to_string())?;
    Ok(format!("wrote {}/{key}", scope.label()))
}

fn tool_memory_read(def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let scope_raw = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or("'scope' is required.")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let scope = parse_memory_scope_mcp(scope_raw, name, &me)?;
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or("'key' is required.")?;
    let entry = memory::memory_read(&scope, key).map_err(|e| e.to_string())?;
    let tags = if entry.tags.is_empty() {
        String::new()
    } else {
        format!(" | tags={:?}", entry.tags)
    };
    Ok(format!(
        "scope: {} | key: {} | title: {} | updated: {}{}\n---\n{}",
        entry.scope.label(),
        entry.key,
        entry.title,
        fmt_ts(entry.updated_ts),
        tags,
        entry.body
    ))
}

fn tool_memory_search(def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            parse_memory_scope_mcp(s, name, &me)
        })
        .transpose()?;
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("'query' is required.")?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50) as usize;
    let hits = memory::memory_search(scope.as_ref(), query).map_err(|e| e.to_string())?;
    if hits.is_empty() {
        return Ok("No memory entries matched.".to_string());
    }
    let mut out = format!("{} memory entr(y/ies) matched:\n", hits.len().min(limit));
    for e in hits.iter().take(limit) {
        let tags = if e.tags.is_empty() {
            String::new()
        } else {
            format!(" | tags={:?}", e.tags)
        };
        out.push_str(&format!(
            "{} | {} | {}{}\n",
            e.scope.label(),
            e.key,
            e.title,
            tags
        ));
    }
    Ok(out)
}

fn tool_memory_list(def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let scope_raw = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or("'scope' is required.")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let scope = parse_memory_scope_mcp(scope_raw, name, &me)?;
    let list = memory::memory_list(&scope).map_err(|e| e.to_string())?;
    if list.is_empty() {
        return Ok(format!("no entries in {}", scope.label()));
    }
    let mut out = format!("Entries in {}:\n", scope.label());
    for e in &list {
        let tags = if e.tags.is_empty() {
            String::new()
        } else {
            format!(" | tags={:?}", e.tags)
        };
        out.push_str(&format!("{} | {}{}\n", e.key, e.title, tags));
    }
    Ok(out)
}

fn tool_memory_delete(def: &Option<String>, args: &Value) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let scope_raw = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or("'scope' is required.")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let scope = parse_memory_scope_mcp(scope_raw, name, &me)?;
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or("'key' is required.")?;
    if memory::memory_delete(&scope, key).map_err(|e| e.to_string())? {
        Ok(format!("deleted {}/{key}", scope.label()))
    } else {
        Ok(format!("not found: {}/{key}", scope.label()))
    }
}

fn tool_review_queue(store: &dyn Store, args: &Value) -> Result<String, String> {
    let filter_str = args.get("filter").and_then(|v| v.as_str()).unwrap_or("all");
    let filter = model::ReviewQueueFilter::from_str(filter_str)?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let items = store
        .review_queue(filter, limit)
        .map_err(|e| e.to_string())?;
    if items.is_empty() {
        return Ok("no review items".to_string());
    }
    let mut out = format!("{} review item(s):\n", items.len());
    for item in items {
        let status = if let Some(ref by) = item.reviewed_by {
            format!("reviewed by {by}")
        } else {
            "pending".to_string()
        };
        out.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            item.id, item.repo, item.author, status, item.pr_url
        ));
    }
    Ok(out)
}

fn tool_review_add(store: &dyn Store, args: &Value) -> Result<String, String> {
    let pr_url = args
        .get("pr_url")
        .and_then(|v| v.as_str())
        .ok_or("'pr_url' is required.")?;
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let author = args.get("author").and_then(|v| v.as_str()).unwrap_or("");
    let repo = args.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let id = store
        .add_review_item(
            pr_url,
            title,
            author,
            repo,
            model::ReviewItemState::Open,
            None,
        )
        .map_err(|e| e.to_string())?;
    Ok(format!("added review item {id}"))
}

fn tool_review_mark(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("'id' is required.")?;
    let reviewer = ident(args, "from", def)?;
    if store
        .mark_reviewed(id, &reviewer)
        .map_err(|e| e.to_string())?
    {
        Ok(format!("marked {id} as reviewed"))
    } else {
        Ok(format!("not found: {id}"))
    }
}

fn tool_review_remove(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("'id' is required.")?;
    if store.remove_review_item(id).map_err(|e| e.to_string())? {
        Ok(format!("removed {id}"))
    } else {
        Ok(format!("not found: {id}"))
    }
}

fn tool_ask_permission(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let to_raw = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("'to' is required (the peer session name to ask).")?;
    let to = bound_ident("to", to_raw)?;
    if model::is_broadcast(&to) {
        return Err(
            "tracked ask is point-to-point; use weave_send for broadcast (broadcast ask is P2)."
                .to_string(),
        );
    }
    let tool = args
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or("'tool' is required.")?;
    let tool_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("");
    let options = format!("{}\n{}", tool, tool_args);
    let body_raw = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let body = if body_raw.is_empty() {
        format!("Requesting permission to run {} {}", tool, tool_args)
    } else {
        body_raw.to_string()
    };
    let (cid, qid) = store
        .ask(
            &from,
            &to,
            None,
            &body,
            model::AskKind::ToolPermission,
            Some(&options),
            None,
        )
        .map_err(|e| e.to_string())?;
    record_delivery_best_effort(
        store,
        qid,
        model::DeliveryRefKind::Ask,
        &to,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &to, &body, injector);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(store, qid, model::DeliveryRefKind::Ask, &to, stage, outcome);
    Ok(format!(
        "Opened permission ask {cid} from '{from}' to '{to}'. {}",
        verdict_sentence(verdict, &to)
    ))
}

fn tool_permission_status(store: &dyn Store, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("'id' is required.")?;
    let timeout = args.get("timeout").and_then(|v| v.as_i64()).unwrap_or(0);
    let (status, answer) = store
        .permission_verdict(id, timeout)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{}: {} (answer: {})",
        id,
        status.as_str(),
        answer.unwrap_or_default()
    ))
}

fn tool_permission_list(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let asks = store
        .list_permissions(&me, limit)
        .map_err(|e| e.to_string())?;
    if asks.is_empty() {
        return Ok("no permission asks".to_string());
    }
    let mut out = format!("{} permission ask(s):\n", asks.len());
    for a in asks {
        let (status, _) = store
            .permission_verdict(&a.id, 0)
            .map_err(|e| e.to_string())?;
        let tool = a
            .options
            .as_ref()
            .and_then(|o| o.lines().next())
            .unwrap_or("?");
        out.push_str(&format!(
            "{} | {} | {} -> {} | {}\n",
            a.id,
            status.as_str(),
            a.asker,
            a.askee,
            tool
        ));
    }
    Ok(out)
}

fn tool_lease_reserve(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let resource = args
        .get("resource")
        .and_then(|v| v.as_str())
        .ok_or("resource required")?;
    let ttl = args
        .get("ttl")
        .and_then(|v| v.as_i64())
        .ok_or("ttl required")?;
    let note = args.get("note").and_then(|v| v.as_str());
    match store.reserve_lease(&me, resource, ttl, note) {
        Ok(lease) => Ok(format!(
            "leased {} (expires {})",
            lease.resource, lease.expires
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn tool_lease_release(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
) -> Result<String, String> {
    let me = ident(args, "me", def)?;
    let resource = args
        .get("resource")
        .and_then(|v| v.as_str())
        .ok_or("resource required")?;
    let ok = store
        .release_lease(&me, resource)
        .map_err(|e| e.to_string())?;
    if ok {
        Ok(format!("released {}", resource))
    } else {
        Err(format!("no active lease for {} held by you", resource))
    }
}

fn tool_lease_list(store: &dyn Store, args: &Value) -> Result<String, String> {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let leases = store.list_leases(limit).map_err(|e| e.to_string())?;
    if leases.is_empty() {
        return Ok("no active leases".to_string());
    }
    let mut out = format!("{} lease(s):\n", leases.len());
    for l in leases {
        out.push_str(&format!(
            "{} | {} | expires {} | {}\n",
            l.resource,
            l.holder,
            l.expires,
            if l.note.is_empty() { "-" } else { &l.note }
        ));
    }
    Ok(out)
}

fn tool_lease_sweep(store: &dyn Store) -> Result<String, String> {
    let n = store.sweep_expired_leases().map_err(|e| e.to_string())?;
    Ok(format!("swept {} expired lease(s)", n))
}

#[cfg(feature = "llm")]
fn tool_thread_summarize(store: &dyn Store, args: &Value) -> Result<String, String> {
    let root_id = args["root_id"]
        .as_i64()
        .ok_or("root_id must be an integer")?;
    let refresh = args["refresh"].as_bool().unwrap_or(false);
    let summary = if refresh {
        None
    } else {
        store.get_summary(root_id).map_err(|e| e.to_string())?
    };
    let text = match summary {
        Some(s) => s.text,
        None => {
            let rows = store.thread(root_id, 200).map_err(|e| e.to_string())?;
            let _thread_text = rows
                .iter()
                .map(|m| format!("{}: {}", m.sender, m.body))
                .collect::<Vec<_>>()
                .join("\n");
            return Err("LLM summarization not yet available via MCP".to_string());
        }
    };
    Ok(text)
}

#[cfg(not(feature = "llm"))]
fn tool_thread_summarize(_store: &dyn Store, _args: &Value) -> Result<String, String> {
    Err("weave was compiled without the llm feature".to_string())
}

#[cfg(feature = "llm")]
fn tool_summarize_text(_args: &Value) -> Result<String, String> {
    Err("weave_summarize_text requires config access not yet wired in MCP".to_string())
}

#[cfg(not(feature = "llm"))]
fn tool_summarize_text(_args: &Value) -> Result<String, String> {
    Err("weave was compiled without the llm feature".to_string())
}

/// WL-049 / ADR-0002: the single token-light `weave_web` dispatcher. ONE tool
/// proxies all 35 `browser_*` obscura ops behind weave's governance plane
/// (deny-by-default permission / lease / job gating). See ADR-0002.
///
/// Governance flow (§4 of the plan):
///   (a) resolve the caller identity;
///   (b) **policy gate (deny-by-default)** — parse `action` → `WebOp`, run the
///       `webpolicy::WebPolicy` decision (op allow-list + SSRF/loopback URL guard).
///       A refused op returns `Err` WITHOUT spawning obscura;
///   (c) optional **lease** (rate / mutual-exclusion) keyed on `web:<host>`;
///   (d) optional **job** record for a durable audit trail (created before forward,
///       terminally stamped after — the append-only event log is the audit);
///   (e) **forward** to obscura via the spawn-and-speak MCP client;
///   (f) return the obscura `content[0].text` (capped).
///
/// `describe:true` returns weave's thin description of the op (args forwarded
/// opaquely; the authoritative schema lives in obscura) WITHOUT spawning obscura or
/// touching the gate — progressive disclosure keeps the 35 schemas out of the
/// standing tool table (ADR-0003).
#[cfg(feature = "obscura")]
fn tool_web(
    store: &dyn Store,
    def: &Option<String>,
    args: &Value,
    injector: &dyn Injector,
) -> Result<String, String> {
    use weave_core::webpolicy::{self, WebPolicy};

    // (a) Identity.
    let me = ident(args, "me", def)?;

    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("'action' is required (a browser op, e.g. \"navigate\").")?;

    // `action:"list"` / `describe` are pure metadata — no spawn, no gate.
    if action == "list" {
        let ops = webpolicy::WEB_OPS.join(", ");
        return Ok(format!(
            "{} web ops available: {ops}",
            webpolicy::WEB_OPS.len()
        ));
    }
    let op_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    if args
        .get("describe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let op =
            webpolicy::WebOp::parse(action).ok_or_else(|| format!("unknown web op {action:?}"))?;
        return Ok(format!(
            "web op {:?} → obscura tool {:?}. Args are forwarded opaquely as a JSON object; \
             the authoritative per-op arg schema lives in obscura (run `obscura mcp` tools/list). \
             Common nav arg: {{\"url\": \"https://…\"}}.",
            op.name(),
            op.obscura_tool()
        ));
    }

    // (b) Policy gate (deny-by-default).
    let cfg = weave_core::config::Config::load();
    let policy = WebPolicy::from_config(&cfg);
    let url = op_args.get("url").and_then(|v| v.as_str());
    let op = policy.decide(action, url).map_err(|d| d.message())?;

    // Defense-in-depth: cap every string arg value (they ride a JSON-RPC frame to
    // the child). Non-string args are forwarded unchanged.
    if let Some(obj) = op_args.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                webpolicy::check_arg(k, s).map_err(|d| d.message())?;
            }
        }
    }

    // (c) Optional lease: when a `lease_ttl` is supplied, reserve a per-host lease so
    // concurrent sessions can mutually-exclude / rate-limit on a target. Released
    // after the op regardless of outcome. Keyed on the validated host (or the op).
    let lease_ttl = args.get("lease_ttl").and_then(|v| v.as_i64());
    let lease_resource = url
        .and_then(webpolicy::url_host)
        .map(|h| format!("web:{h}"))
        .unwrap_or_else(|| format!("web:{}", op.name()));
    if let Some(ttl) = lease_ttl {
        store
            .reserve_lease(&me, &lease_resource, ttl, Some("weave_web"))
            .map_err(|e| format!("web resource lease failed: {e}"))?;
    }

    // (d) Optional job audit: when `audit:true`, record a durable job row before the
    // forward and stamp it terminally after (the append-only event log is the trail).
    let audit = args.get("audit").and_then(|v| v.as_bool()).unwrap_or(false);
    let job_id = if audit {
        let spec = model::JobSpec {
            title: format!("web {} {}", op.name(), url.unwrap_or("")),
            description: None,
            kind: Some("web".to_string()),
            owner: Some(me.clone()),
            assignee: None,
            circle: None,
            prompt: None,
            correlation_id: None,
            source_kind: Some("weave_web".to_string()),
            source_id: url.map(str::to_string),
            scope: None,
            visibility: None,
            deadline_at: None,
            expires_at: None,
        };
        match store.create_job(&me, spec) {
            Ok(job) => Some(job.id),
            Err(e) => {
                log(&format!("web audit job creation failed (continuing): {e}"));
                None
            }
        }
    } else {
        None
    };

    // (e) Forward to obscura (spawn-and-speak; lazy spawn + reuse).
    let outcome = crate::obscura::call(&cfg, &op.obscura_tool(), &op_args);

    // (c′) release the lease (best-effort).
    if lease_ttl.is_some() {
        let _ = store.release_lease(&me, &lease_resource);
    }

    // (d′) stamp the audit job terminally.
    if let Some(jid) = job_id {
        let (state, note) = match &outcome {
            Ok(_) => (model::JobState::Completed, "web op ok".to_string()),
            Err(e) => (model::JobState::Failed, format!("web op failed: {e}")),
        };
        let patch = model::JobPatch {
            state: Some(state),
            state_reason: None,
            phase: None,
            progress_note: Some(note),
            result_summary: None,
            result_json: None,
            error_json: None,
            artifacts_json: None,
        };
        let _ = store.update_job(&jid, None, patch);
    }

    let _ = injector; // governance path does not inject; signature parity with peers.

    // (f) return the obscura payload, capped to a weave body-class limit.
    let text = outcome?;
    let capped: String = text.chars().take(store::MAX_BODY).collect();
    Ok(capped)
}

#[cfg(not(feature = "obscura"))]
fn tool_web(
    _store: &dyn Store,
    _def: &Option<String>,
    _args: &Value,
    _injector: &dyn Injector,
) -> Result<String, String> {
    Err("weave was compiled without the obscura feature (governed web access).".to_string())
}

/// WL-049: CLI entrypoint for `weave web` — routes through the SAME `tool_web`
/// governance path as the MCP tool (deny-by-default policy / lease / job gate). The
/// bin builds the `args` Value (action/args/lease_ttl/audit) and weave-mcp runs it,
/// so the CLI and MCP surfaces share ONE code path (ADR-0003 CLI parity). The
/// governance path ignores the injector, so a no-op stand-in is supplied here rather
/// than threading the bin's `RealInjector` through.
#[cfg(feature = "obscura")]
pub fn run_web(store: &dyn Store, me: &Option<String>, args: &Value) -> Result<String, String> {
    struct NoInjector;
    impl Injector for NoInjector {
        fn detect_target(&self) -> Target {
            Target::default()
        }
        fn target_alive(&self, _t: &Target) -> bool {
            false
        }
        fn inject_mode(&self, _t: &Target, _b: &str, _m: Nudge) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn capability(&self, _t: &Target) -> Capability {
            Capability::NotInjectable
        }
        fn have(&self, _name: &str) -> bool {
            false
        }
        fn id_valid(&self, _mux: weave_inject::Mux, _id: &str) -> bool {
            false
        }
        fn git_tags(
            &self,
            _cwd: &std::path::Path,
        ) -> anyhow::Result<weave_core::model::WorktreeTags> {
            Ok(weave_core::model::WorktreeTags::default())
        }
    }
    tool_web(store, me, args, &NoInjector)
}

/// WL-049: stop and reap the cached obscura child (`weave web --stop`).
#[cfg(feature = "obscura")]
pub fn stop_web() {
    crate::obscura::stop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use weave_core::model::WorktreeTags;
    use weave_inject::{Mux, SpawnOutcome};

    /// A mock injector must be usable as `&dyn Injector` so `serve` is testable without
    /// a real mux environment. This is the compile-time + coercion proof for the trait
    /// abstraction added during the workspace split.
    #[test]
    fn mock_injector_implements_trait() {
        struct MockInjector;
        impl Injector for MockInjector {
            fn detect_target(&self) -> Target {
                Target::none()
            }
            fn target_alive(&self, _target: &Target) -> bool {
                false
            }
            fn inject_mode(
                &self,
                _target: &Target,
                _body: &str,
                _mode: Nudge,
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn capability(&self, _target: &Target) -> Capability {
                Capability::NotInjectable
            }
            fn have(&self, _name: &str) -> bool {
                false
            }
            fn id_valid(&self, _mux: Mux, _id: &str) -> bool {
                false
            }
            fn git_tags(&self, _cwd: &std::path::Path) -> anyhow::Result<WorktreeTags> {
                Ok(WorktreeTags::default())
            }
        }
        let mock = MockInjector;
        let _dyn_ref: &dyn Injector = &mock;
    }

    // ---- WL-047 spawn/kill MCP tool tests -----------------------------------

    use std::sync::Mutex;

    /// One recorded `spawn` call: the exact arguments the MCP tool threaded down.
    struct SpawnRecord {
        mux: Mux,
        cwd: String,
        name: String,
        cert: String,
        argv: Vec<String>,
        window: bool,
    }

    /// A recording fake injector: captures the exact `spawn`/`kill` calls (env +
    /// argv + target) and returns a scripted [`SpawnOutcome`], so the MCP tools are
    /// driven without a real mux. Overrides ONLY the trait methods the tools touch;
    /// the rest panic if reached (they must not be on these code paths).
    #[derive(Default)]
    struct RecordingInjector {
        spawn_calls: Mutex<Vec<SpawnRecord>>,
        kill_calls: Mutex<Vec<Target>>,
        /// The target id the fake mux "echoes" back from a spawn (empty ⇒ none).
        echo_target: String,
        detect_mux: Mux,
    }
    impl Injector for RecordingInjector {
        fn detect_target(&self) -> Target {
            Target {
                mux: self.detect_mux,
                id: "%1".to_string(),
                socket: String::new(),
            }
        }
        fn target_alive(&self, _t: &Target) -> bool {
            true
        }
        fn inject_mode(&self, _t: &Target, _b: &str, _m: Nudge) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn capability(&self, _t: &Target) -> Capability {
            Capability::Live
        }
        fn have(&self, _n: &str) -> bool {
            true
        }
        fn id_valid(&self, mux: Mux, id: &str) -> bool {
            weave_inject::id_valid(mux, id)
        }
        fn git_tags(&self, _cwd: &std::path::Path) -> anyhow::Result<WorktreeTags> {
            Ok(WorktreeTags::default())
        }
        #[allow(clippy::too_many_arguments)]
        fn spawn(
            &self,
            mux: Mux,
            cwd: &str,
            name: &str,
            cert: &str,
            _circle: &str,
            argv_child: &[String],
            window: bool,
        ) -> anyhow::Result<SpawnOutcome> {
            self.spawn_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(SpawnRecord {
                mux,
                cwd: cwd.to_string(),
                name: name.to_string(),
                cert: cert.to_string(),
                argv: argv_child.to_vec(),
                window,
            });
            Ok(SpawnOutcome {
                launched: true,
                target: self.echo_target.clone(),
            })
        }
        fn kill(&self, target: &Target) -> anyhow::Result<bool> {
            self.kill_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(target.clone());
            Ok(true)
        }
    }

    /// A unique temp store on disk, built for WHICHEVER backend the crate is compiled
    /// with (the MCP layer holds `&dyn Store`, so the tools are backend-agnostic and
    /// must pass under both the default `sqlite` and the `--features libsql` builds).
    fn store() -> Box<dyn Store> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weave-mcp-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        #[cfg(feature = "sqlite")]
        {
            Box::new(weave_core::store::SqliteStore::open(&path).unwrap())
        }
        #[cfg(all(feature = "libsql", not(feature = "sqlite")))]
        {
            let cfg = weave_core::config::Config {
                db: Some(path.to_string_lossy().into_owned()),
                backend: Some("libsql".to_string()),
                ..weave_core::config::Config::default()
            };
            Box::new(weave_core::store_libsql::LibsqlStore::open(&cfg).unwrap())
        }
    }

    fn pull_consent() -> PullConsent {
        PullConsent {
            from: vec![],
            inject_pulled: true,
            allow_inject_from: None,
            policy: store::VerifyPolicy::default(),
        }
    }

    fn call(name: &str, args: Value, st: &dyn Store, inj: &dyn Injector) -> Result<String, String> {
        call_tool(
            st,
            &None,
            None,
            &[],
            &pull_consent(),
            name,
            &args,
            inj,
            true,
        )
    }

    /// `WEAVE_SPAWN_DIRS` is process-global; serialize the env-touching spawn tests so
    /// parallel cases can't clobber each other's allowlist.
    static SPAWN_ENV_LOCK: Mutex<()> = Mutex::new(());

    // ---- WL-049 / ADR-0002 governed web access (weave_web) -------------------

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_is_registered_and_dangerous() {
        // The single token-light dispatcher is present in the operation catalog…
        // (WL-050: the standing surface is now the `weave` meta-tool; weave_web is
        // reachable via it — its one-dispatcher property is a catalog invariant.)
        let listed = Value::Array(tool_catalog());
        let has = listed
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("weave_web"));
        assert!(has, "weave_web must be in tools() under --features obscura");
        // …and is gated as dangerous (blocked in safe HTTP mode).
        assert!(is_dangerous_tool("weave_web"));
        // The standing table grew by exactly ONE entry — the whole point of the
        // single dispatcher is that the 35 browser_* ops are NOT 35 standing tools.
        let web_entries = listed
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n == "weave_web")
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(web_entries, 1, "obscura must add ONE tool, not 35");
        // No per-op browser_* tool leaked into the standing table.
        let leaked_browser = listed.as_array().unwrap().iter().any(|t| {
            t.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.starts_with("browser_"))
                .unwrap_or(false)
        });
        assert!(
            !leaked_browser,
            "no per-op browser_* tool may appear in the standing table"
        );
        // The schema's properties match what tool_web actually reads.
        let schema = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("weave_web"))
            .unwrap();
        let props = schema["inputSchema"]["properties"].as_object().unwrap();
        for k in ["me", "action", "args", "describe", "lease_ttl", "audit"] {
            assert!(props.contains_key(k), "weave_web schema must expose {k:?}");
        }
        assert_eq!(schema["inputSchema"]["required"][0], "action");
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_list_action_needs_no_obscura() {
        // `action:"list"` is pure metadata: it must succeed without any obscura
        // binary present and enumerate the 35 ops.
        let st = store();
        let inj = no_injector();
        let out = call(
            "weave_web",
            json!({"me": "tester", "action": "list"}),
            st.as_ref(),
            &inj,
        )
        .expect("list action should succeed");
        assert!(out.contains("35 web ops"), "got: {out}");
        assert!(out.contains("navigate"), "got: {out}");
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_describe_needs_no_obscura() {
        let st = store();
        let inj = no_injector();
        let out = call(
            "weave_web",
            json!({"me": "tester", "action": "navigate", "describe": true}),
            st.as_ref(),
            &inj,
        )
        .expect("describe should succeed");
        assert!(out.contains("browser_navigate"), "got: {out}");
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_deny_by_default_does_not_spawn() {
        // With no allow-ops policy configured, a real web op is refused BEFORE any
        // obscura spawn. Point config discovery at an empty dir (no config.toml) so
        // the policy is genuinely unset, and the obscura bin at a name that does not
        // resolve — proving the deny happens first (a spawn would error differently).
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty_cfg = std::env::temp_dir().join(format!("weave-noconf-{}", std::process::id()));
        let _xdg =
            weave_core::testenv::EnvVarGuard::set("XDG_CONFIG_HOME", &empty_cfg.to_string_lossy());
        let _bin =
            weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_BIN", "definitely-not-a-binary");
        let st = store();
        let inj = no_injector();
        let err = call(
            "weave_web",
            json!({"me": "tester", "action": "navigate", "args": {"url": "https://example.com"}}),
            st.as_ref(),
            &inj,
        )
        .expect_err("deny-by-default must refuse");
        assert!(
            err.contains("not allowed by policy"),
            "expected a policy refusal (not a spawn error), got: {err}"
        );
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_ssrf_blocked_before_spawn() {
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty_cfg =
            std::env::temp_dir().join(format!("weave-noconf-ssrf-{}", std::process::id()));
        let _xdg =
            weave_core::testenv::EnvVarGuard::set("XDG_CONFIG_HOME", &empty_cfg.to_string_lossy());
        let _ops = weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_ALLOW_OPS", "navigate");
        let _bin =
            weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_BIN", "definitely-not-a-binary");
        let st = store();
        let inj = no_injector();
        let err = call(
            "weave_web",
            json!({"me": "tester", "action": "navigate", "args": {"url": "http://127.0.0.1"}}),
            st.as_ref(),
            &inj,
        )
        .expect_err("SSRF target must be refused");
        assert!(err.contains("SSRF guard"), "got: {err}");
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_unknown_action_is_error_no_spawn() {
        // An unknown op is refused by the deny-by-default parse gate (even with a
        // wildcard allow-list) BEFORE any obscura spawn — a clean error, never a
        // panic or a spawn of a binary that does not resolve.
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty_cfg =
            std::env::temp_dir().join(format!("weave-noconf-unk-{}", std::process::id()));
        let _xdg =
            weave_core::testenv::EnvVarGuard::set("XDG_CONFIG_HOME", &empty_cfg.to_string_lossy());
        let _ops = weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_ALLOW_OPS", "*");
        let _bin =
            weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_BIN", "definitely-not-a-binary");
        let st = store();
        let inj = no_injector();
        let err = call(
            "weave_web",
            json!({"me": "tester", "action": "browser_exec_shell"}),
            st.as_ref(),
            &inj,
        )
        .expect_err("unknown op must be refused");
        assert!(
            err.contains("unknown web op"),
            "expected unknown-op refusal, got: {err}"
        );
    }

    #[cfg(feature = "obscura")]
    #[test]
    fn weave_web_obscura_missing_is_graceful_error() {
        // Op IS allowed (and URL is SSRF-safe) so the gate passes — but the obscura
        // binary does not resolve to a trusted dir. weave must surface a clean error
        // (binary not found), never a panic. This is the "allowed but obscura-missing"
        // path distinct from deny-by-default.
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty_cfg =
            std::env::temp_dir().join(format!("weave-noconf-miss-{}", std::process::id()));
        let _xdg =
            weave_core::testenv::EnvVarGuard::set("XDG_CONFIG_HOME", &empty_cfg.to_string_lossy());
        let _ops = weave_core::testenv::EnvVarGuard::set("WEAVE_OBSCURA_ALLOW_OPS", "navigate");
        // A bin name that cannot resolve in any trusted dir.
        let _bin = weave_core::testenv::EnvVarGuard::set(
            "WEAVE_OBSCURA_BIN",
            "weave-no-such-obscura-binary-xyz",
        );
        // Ensure no stale trusted-dir override leaks the bin in.
        let _mux = weave_core::testenv::EnvVarGuard::set("WEAVE_MUX_DIR", "/nonexistent-weave-dir");
        let st = store();
        let inj = no_injector();
        let err = call(
            "weave_web",
            json!({"me": "tester", "action": "navigate", "args": {"url": "https://example.com"}}),
            st.as_ref(),
            &inj,
        )
        .expect_err("a missing obscura binary must be a clean error");
        assert!(
            err.contains("not found in a trusted directory") || err.contains("obscura"),
            "expected a clean obscura-missing error, got: {err}"
        );
    }

    /// A no-op injector for the web-tool tests (the governance path never injects).
    #[cfg(feature = "obscura")]
    fn no_injector() -> impl Injector {
        struct N;
        impl Injector for N {
            fn detect_target(&self) -> Target {
                Target::none()
            }
            fn target_alive(&self, _t: &Target) -> bool {
                false
            }
            fn inject_mode(&self, _t: &Target, _b: &str, _m: Nudge) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn capability(&self, _t: &Target) -> Capability {
                Capability::NotInjectable
            }
            fn have(&self, _n: &str) -> bool {
                false
            }
            fn id_valid(&self, _mux: weave_inject::Mux, _id: &str) -> bool {
                false
            }
            fn git_tags(
                &self,
                _cwd: &std::path::Path,
            ) -> anyhow::Result<weave_core::model::WorktreeTags> {
                Ok(weave_core::model::WorktreeTags::default())
            }
        }
        N
    }

    /// Happy path: `weave_spawn_peer` records the spawn with the exact child argv, the
    /// resolved cwd, the minted cert, and — since the fake echoes a target id — the
    /// peer is pre-registered with that cert. Asserts env-thread correctness via the
    /// recorded (name, cert) the runner turns into WEAVE_SESSION / WEAVE_BIRTH_CERT.
    #[test]
    fn spawn_peer_happy_path_records_and_registers() {
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let allow = std::env::temp_dir().join(format!("weave-spawn-ok-{}", std::process::id()));
        std::fs::create_dir_all(&allow).unwrap();
        let allow_real = std::fs::canonicalize(&allow).unwrap();
        std::env::set_var("WEAVE_SPAWN_DIRS", &allow_real);

        let st = store();
        let inj = RecordingInjector {
            echo_target: "%7".to_string(),
            detect_mux: Mux::Tmux,
            ..Default::default()
        };
        let out = call(
            "weave_spawn_peer",
            json!({"name":"kid","cmd":["echo","hi"],"cwd": allow_real.to_string_lossy()}),
            st.as_ref(),
            &inj,
        )
        .expect("spawn should succeed for an allowed cwd");
        assert!(out.contains("kid"), "result names the spawned peer: {out}");
        assert!(
            out.contains("birth-cert"),
            "result discloses the cert: {out}"
        );

        let calls = inj.spawn_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.len(), 1, "exactly one spawn fired");
        let rec = &calls[0];
        assert_eq!(rec.mux, Mux::Tmux);
        assert_eq!(rec.cwd, allow_real.to_string_lossy());
        assert_eq!(rec.name, "kid");
        assert!(!rec.cert.is_empty(), "a birth cert was minted + threaded");
        assert_eq!(rec.argv, vec!["echo".to_string(), "hi".to_string()]);
        assert!(!rec.window, "pane by default");

        // The echoed id ⇒ the peer is registered with the minted cert.
        let peer = st.get_peer("kid").unwrap().expect("peer pre-registered");
        assert_eq!(peer.target, "%7");
        assert_eq!(
            st.get_birth_cert("kid").unwrap().unwrap(),
            rec.cert,
            "the registered cert matches the one threaded into the child env"
        );
        std::env::remove_var("WEAVE_SPAWN_DIRS");
    }

    /// Disallowed cwd: with no allowlist (deny-by-default), the spawn is refused as an
    /// Err (isError at the protocol seam) and NO spawn call fires.
    #[test]
    fn spawn_peer_disallowed_cwd_is_error() {
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("WEAVE_SPAWN_DIRS");
        let st = store();
        let inj = RecordingInjector {
            detect_mux: Mux::Tmux,
            ..Default::default()
        };
        let tmp = std::env::temp_dir();
        let err = call(
            "weave_spawn_peer",
            json!({"name":"kid","cmd":["echo"],"cwd": tmp.to_string_lossy()}),
            st.as_ref(),
            &inj,
        )
        .expect_err("deny-by-default must refuse the spawn");
        assert!(
            err.contains("spawn_allowed_dirs") || err.contains("refusing to spawn"),
            "error explains the allowlist denial: {err}"
        );
        assert!(
            inj.spawn_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "no spawn fires when the cwd is denied"
        );
        assert!(st.get_peer("kid").unwrap().is_none(), "no phantom peer row");
    }

    /// Spawning over an already-registered name is refused (Err) before any launch.
    #[test]
    fn spawn_peer_existing_name_is_error() {
        let _g = SPAWN_ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let allow = std::env::temp_dir().join(format!("weave-spawn-dup-{}", std::process::id()));
        std::fs::create_dir_all(&allow).unwrap();
        let allow_real = std::fs::canonicalize(&allow).unwrap();
        std::env::set_var("WEAVE_SPAWN_DIRS", &allow_real);
        let st = store();
        st.register_peer_full(
            "taken", "tmux", "%1", "", None, None, "h", "", "", "", "default", None, "",
        )
        .unwrap();
        let inj = RecordingInjector {
            detect_mux: Mux::Tmux,
            ..Default::default()
        };
        let err = call(
            "weave_spawn_peer",
            json!({"name":"taken","cmd":["echo"],"cwd": allow_real.to_string_lossy()}),
            st.as_ref(),
            &inj,
        )
        .expect_err("cannot spawn over a live peer");
        assert!(err.contains("already registered"), "{err}");
        assert!(inj.spawn_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
        std::env::remove_var("WEAVE_SPAWN_DIRS");
    }

    /// `weave_kill_peer` for an unknown peer ⇒ Err (isError), no kill call.
    #[test]
    fn kill_peer_unknown_is_error() {
        let st = store();
        let inj = RecordingInjector::default();
        let err = call(
            "weave_kill_peer",
            json!({"name":"ghost"}),
            st.as_ref(),
            &inj,
        )
        .expect_err("unknown peer must error");
        assert!(err.contains("no registered peer"), "{err}");
        assert!(inj.kill_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
    }

    /// `weave_kill_peer` happy path: a registered tmux peer is killed via the trait.
    #[test]
    fn kill_peer_records_kill() {
        let st = store();
        st.register_peer_full(
            "victim", "tmux", "%3", "", None, None, "h", "", "", "", "default", None, "",
        )
        .unwrap();
        let inj = RecordingInjector::default();
        let out = call(
            "weave_kill_peer",
            json!({"name":"victim"}),
            st.as_ref(),
            &inj,
        )
        .unwrap();
        assert!(out.contains("Killed"), "{out}");
        let calls = inj.kill_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].mux, Mux::Tmux);
        assert_eq!(calls[0].id, "%3");
    }

    /// Kill on an unsupported mux (iterm2/none) ⇒ graceful Ok message, no kill call.
    #[test]
    fn kill_peer_unsupported_mux_is_graceful() {
        let st = store();
        st.register_peer_full(
            "it", "iterm2", "anything", "", None, None, "h", "", "", "", "default", None, "",
        )
        .unwrap();
        let inj = RecordingInjector::default();
        let out = call("weave_kill_peer", json!({"name":"it"}), st.as_ref(), &inj)
            .expect("unsupported mux is graceful, not an error");
        assert!(
            out.contains("not supported"),
            "graceful unsupported message: {out}"
        );
        assert!(
            inj.kill_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "no kill argv runs on an unsupported mux"
        );
    }

    /// Both spawn/kill are in DANGEROUS_TOOLS, so a safe-mode (`dangerous=false`)
    /// HTTP `tools/call` is rejected at `dispatch_request` with the disabled message;
    /// with `--dangerous` (`dangerous=true`) the gate lets the call through.
    #[test]
    fn spawn_kill_blocked_in_safe_http_mode() {
        assert!(is_dangerous_tool("weave_spawn_peer"));
        assert!(is_dangerous_tool("weave_kill_peer"));
        let st = store();
        let inj = RecordingInjector::default();
        for tool in ["weave_spawn_peer", "weave_kill_peer"] {
            let req = json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":tool,"arguments":{"name":"x"}}
            });
            let reply = dispatch_request(
                st.as_ref(),
                &None,
                None,
                &[],
                &pull_consent(),
                &req,
                &inj,
                /* dangerous = */ false,
            )
            .expect("a reply is produced");
            assert!(
                reply.contains("disabled in safe HTTP mode"),
                "{tool} must be gated in safe mode: {reply}"
            );
        }
        // No spawn/kill ever fired — the gate blocks before call_tool.
        assert!(inj.spawn_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
        assert!(inj.kill_calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
    }

    // ---- WL-050 / ADR-0003 token-light progressive-disclosure MCP -----------

    /// `WEAVE_MCP_EAGER` is process-global; serialize the two tests that mutate it so
    /// a parallel run can't observe the standing surface mid-flip.
    static MCP_EAGER_LOCK: Mutex<()> = Mutex::new(());

    /// Default (progressive disclosure): the standing `tools/list` surface is exactly
    /// ONE tool — the `weave` meta-tool — not the dozens of flat ops. This is the whole
    /// token-light point: a bounded standing context cost regardless of op count.
    #[test]
    fn progressive_default_surface_is_just_the_meta_tool() {
        let _g = MCP_EAGER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("WEAVE_MCP_EAGER");
        let listed = tools();
        let arr = listed.as_array().expect("tools() is an array");
        assert_eq!(arr.len(), 1, "standing surface must be a single meta-tool");
        assert_eq!(
            arr[0].get("name").and_then(|v| v.as_str()),
            Some("weave"),
            "the one standing tool is the `weave` meta-tool"
        );
        // And the meta-tool is itself token-light: it does not inline the catalog.
        assert!(
            arr[0].get("inputSchema").is_some(),
            "meta-tool carries its own small schema"
        );
        // The catalog (full op set) is strictly larger — it is reached on demand.
        assert!(
            tool_catalog().len() > 1,
            "catalog holds the full operation set"
        );
    }

    /// WL-051 / ADR-0003: the **standing-token budget** is enforced. The default
    /// `tools/list` payload must serialize under [`MAX_STANDING_TOOLS_BYTES`] no matter
    /// how many operations exist — this is the automated half of the `token-light`
    /// invariant. A regression that puts flat tools back into the standing table (≈180 KB)
    /// or piles on standing dispatchers trips this immediately.
    #[test]
    fn standing_mcp_surface_is_within_token_budget() {
        let _g = MCP_EAGER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("WEAVE_MCP_EAGER");
        let listed = tools();
        let bytes = serde_json::to_string(&listed)
            .expect("serialize tools")
            .len();
        assert!(
            bytes <= MAX_STANDING_TOOLS_BYTES,
            "standing MCP surface is {bytes} bytes, over the {MAX_STANDING_TOOLS_BYTES}-byte \
             token-light budget (ADR-0003). Adding a feature must not add standing tokens — \
             expose new ops via the `weave` meta-tool's catalog, not new standing tools."
        );
        // It is the meta-tool, not a flat table: a tiny tool count.
        let n = listed.as_array().map(|a| a.len()).unwrap_or(usize::MAX);
        assert!(
            n <= 3,
            "standing surface should be a handful of tools (progressive disclosure), got {n}"
        );
    }

    /// Eager-flat mode (`WEAVE_MCP_EAGER=1`) restores the full standing table, byte-for-byte
    /// the catalog — the backward-compatible path for harnesses that require flat tools.
    #[test]
    fn eager_mode_restores_the_full_flat_table() {
        let _g = MCP_EAGER_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("WEAVE_MCP_EAGER", "1");
        let listed = tools();
        let n = listed.as_array().map(|a| a.len()).unwrap_or(0);
        std::env::remove_var("WEAVE_MCP_EAGER");
        assert_eq!(
            n,
            tool_catalog().len(),
            "eager surface == full catalog (no op dropped)"
        );
        assert!(n > 1, "eager table is the full flat set");
    }

    /// `mode=search` finds ops by keyword over name + description, with compact summaries.
    #[test]
    fn meta_search_finds_ops_by_keyword() {
        let st = store();
        let inj = RecordingInjector::default();
        let out = call(
            "weave",
            json!({"mode":"search","query":"inbox"}),
            st.as_ref(),
            &inj,
        )
        .expect("search succeeds");
        let v: Value = serde_json::from_str(&out).expect("search returns JSON");
        let names: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert!(
            names.contains(&"weave_inbox"),
            "search 'inbox' surfaces weave_inbox: {names:?}"
        );
        // Empty query enumerates everything (an index), still bounded by `limit`.
        let all = call(
            "weave",
            json!({"mode":"search","query":"","limit":5}),
            st.as_ref(),
            &inj,
        )
        .unwrap();
        let av: Value = serde_json::from_str(&all).unwrap();
        assert_eq!(av["matches"].as_array().unwrap().len(), 5, "limit honored");
    }

    /// `mode=list` enumerates the full operation set.
    #[test]
    fn meta_list_enumerates_every_op() {
        let st = store();
        let inj = RecordingInjector::default();
        let out = call("weave", json!({"mode":"list"}), st.as_ref(), &inj).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["count"].as_u64().unwrap() as usize,
            tool_catalog().len(),
            "list returns every catalog op"
        );
    }

    /// `mode=describe` returns one op's full schema; the `weave_` prefix is optional;
    /// an unknown op is an error (not a silent empty).
    #[test]
    fn meta_describe_returns_schema_or_errors() {
        let st = store();
        let inj = RecordingInjector::default();
        // Bare name resolves through the `weave_` prefix.
        let out = call(
            "weave",
            json!({"mode":"describe","name":"send"}),
            st.as_ref(),
            &inj,
        )
        .expect("describe succeeds");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"].as_str(), Some("weave_send"));
        let req: Vec<&str> = v["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str())
            .collect();
        assert!(req.contains(&"to") && req.contains(&"body"), "{req:?}");
        // Unknown op → error.
        let err = call(
            "weave",
            json!({"mode":"describe","name":"does_not_exist"}),
            st.as_ref(),
            &inj,
        )
        .unwrap_err();
        assert!(err.contains("unknown operation"), "{err}");
    }

    // ---- WL-038: ephemeral TTL on weave_send ------------------------------

    /// `weave_send {ttl}` stamps an absolute expiry on the persisted message.
    #[test]
    fn weave_send_ttl_stamps_expiry() {
        let st = store();
        let inj = RecordingInjector::default();
        let out = call(
            "weave_send",
            json!({"from":"a","to":"b","body":"ephemeral","ttl":600}),
            st.as_ref(),
            &inj,
        )
        .expect("send succeeds");
        assert!(out.contains("Sent message"));
        let hist = st.history("b", None, 50).unwrap();
        let m = hist
            .iter()
            .find(|m| m.body == "ephemeral")
            .expect("persisted");
        let exp = m.expires_at.expect("ttl stamped an expiry");
        let now = weave_core::model::now();
        assert!(
            exp > now + 500 && exp <= now + 600,
            "expiry {exp} vs now {now}"
        );
    }

    /// `weave_send {ttl: 0}` is rejected at the seam (cap guard), no row written.
    #[test]
    fn weave_send_ttl_zero_is_rejected() {
        let st = store();
        let inj = RecordingInjector::default();
        let err = call(
            "weave_send",
            json!({"from":"a","to":"b","body":"x","ttl":0}),
            st.as_ref(),
            &inj,
        )
        .unwrap_err();
        assert!(err.contains("ttl"), "{err}");
        assert!(
            st.history("b", None, 50).unwrap().is_empty(),
            "no row on reject"
        );
    }

    /// The catalog `weave_send` schema now lists `ttl` (zero standing-token cost —
    /// it lives only in the meta-tool catalog, NOT as a new standing tool).
    #[test]
    fn catalog_weave_send_lists_ttl() {
        let send = tool_catalog()
            .into_iter()
            .find(|t| t["name"] == "weave_send")
            .expect("weave_send in catalog");
        assert!(
            send["inputSchema"]["properties"].get("ttl").is_some(),
            "weave_send catalog schema must expose ttl"
        );
    }

    /// WL-039: `dedupIdle` is exposed on the `weave_notify` CATALOG op (progressive
    /// disclosure), never as a new standing tool — the standing budget stays green.
    #[test]
    fn catalog_weave_notify_lists_dedup_idle() {
        let notify = tool_catalog()
            .into_iter()
            .find(|t| t["name"] == "weave_notify")
            .expect("weave_notify in catalog");
        assert!(
            notify["inputSchema"]["properties"]
                .get("dedupIdle")
                .is_some(),
            "weave_notify catalog schema must expose dedupIdle"
        );
    }

    /// `mode=call` dispatches to exactly the same handler as a direct flat call — the
    /// meta-tool is a faithful gateway, not a reimplementation.
    #[test]
    fn meta_call_matches_direct_dispatch() {
        let st = store();
        let inj = RecordingInjector::default();
        let direct = call("weave_peers", json!({}), st.as_ref(), &inj).unwrap();
        let viameta = call(
            "weave",
            json!({"mode":"call","name":"peers","arguments":{}}),
            st.as_ref(),
            &inj,
        )
        .unwrap();
        assert_eq!(direct, viameta, "meta call == direct call for weave_peers");
    }

    /// `mode=call` refuses to target the meta-tool itself (no recursion), and rejects
    /// unknown ops with the canonical catch-all (proving it routes through `call_tool`).
    #[test]
    fn meta_call_guards_recursion_and_unknown() {
        let st = store();
        let inj = RecordingInjector::default();
        let rec = call(
            "weave",
            json!({"mode":"call","name":"weave"}),
            st.as_ref(),
            &inj,
        )
        .unwrap_err();
        assert!(rec.contains("cannot target the 'weave' meta-tool"), "{rec}");
        let unk = call(
            "weave",
            json!({"mode":"call","name":"weave_nope","arguments":{}}),
            st.as_ref(),
            &inj,
        )
        .unwrap_err();
        assert!(unk.contains("Unknown tool"), "{unk}");
    }

    /// An unknown/missing `mode` is a clean error, not a panic or a silent no-op.
    #[test]
    fn meta_rejects_bad_mode() {
        let st = store();
        let inj = RecordingInjector::default();
        assert!(call("weave", json!({}), st.as_ref(), &inj)
            .unwrap_err()
            .contains("missing 'mode'"));
        assert!(
            call("weave", json!({"mode":"frobnicate"}), st.as_ref(), &inj)
                .unwrap_err()
                .contains("unknown mode")
        );
    }

    /// The meta-tool's `call` mode is NOT a way around the safe-HTTP destructive-op gate:
    /// in safe mode (`dangerous=false`) a dangerous inner op is rejected exactly as the
    /// flat path rejects it (the gate is on the inner op, not the wrapper).
    #[test]
    fn meta_call_preserves_safe_http_gate() {
        let st = store();
        let inj = RecordingInjector::default();
        // `weave` itself is NOT dangerous (so it lists/searches in safe mode)…
        assert!(!is_dangerous_tool("weave"));
        // …but a dangerous inner op routed via meta=call must still be blocked.
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"weave","arguments":{"mode":"call","name":"weave_clear","arguments":{"scope":"all","confirm":true}}}
        });
        let reply = dispatch_request(
            st.as_ref(),
            &None,
            None,
            &[],
            &pull_consent(),
            &req,
            &inj,
            /* dangerous = */ false,
        )
        .expect("a reply is produced");
        assert!(
            reply.contains("disabled in safe HTTP mode"),
            "dangerous inner op must be gated via meta call too: {reply}"
        );
    }

    /// Catalog ↔ dispatch completeness: every op the meta-tool can `list`/`describe`
    /// is actually dispatchable by `call_tool` — none returns the "Unknown tool"
    /// catch-all. Guards against a catalog entry whose dispatch arm was never wired
    /// (or was removed), which would otherwise only surface at runtime for an agent.
    #[test]
    fn every_catalog_op_is_dispatchable() {
        let st = store();
        let inj = RecordingInjector::default();
        for t in tool_catalog() {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!name.is_empty(), "catalog entry without a name");
            // Empty args → each handler validates required args BEFORE any side effect,
            // so this never spawns/writes; we only assert it is not the unknown-tool arm.
            let r = call_tool(
                st.as_ref(),
                &None,
                None,
                &[],
                &pull_consent(),
                name,
                &json!({}),
                &inj,
                true,
            );
            if let Err(e) = r {
                assert!(
                    !e.starts_with("Unknown tool:"),
                    "catalog op '{name}' has no dispatch arm: {e}"
                );
            }
        }
    }

    /// The pure verdict→stage fold is exhaustive and stable: each P1 verdict token
    /// maps to the documented (stage, outcome), and an unrecognized token degrades to
    /// the safe `Queued/Ok` (never a panic). This locks the mapping the trace relies on.
    #[test]
    fn verdict_to_stage_is_exhaustive() {
        use model::{DeliveryOutcome, DeliveryStage};
        assert_eq!(
            verdict_to_stage("transport_delivered"),
            (DeliveryStage::Injected, DeliveryOutcome::Ok)
        );
        assert_eq!(
            verdict_to_stage("recipient_not_injectable"),
            (DeliveryStage::NotInjectable, DeliveryOutcome::Ok)
        );
        assert_eq!(
            verdict_to_stage("queued_next_turn"),
            (DeliveryStage::Queued, DeliveryOutcome::Ok)
        );
        assert_eq!(
            verdict_to_stage("ambiguous_target_queued"),
            (
                DeliveryStage::NotInjectable,
                DeliveryOutcome::AmbiguousTarget
            )
        );
        // Unknown token degrades safely to queued (the message is in the store).
        assert_eq!(
            verdict_to_stage("anything_else"),
            (DeliveryStage::Queued, DeliveryOutcome::Ok)
        );
    }
}
