//! MCP stdio server: newline-delimited JSON-RPC 2.0 on stdin/stdout. Exposes
//! weave's messaging tools. On send, if the recipient is a registered injectable
//! peer, a live nudge is pushed into their pane via the native injector.
//!
//! stdout is reserved for protocol messages; all logging goes to stderr.

use crate::config::StoreSource;
use crate::inject::{self, Nudge, Target};
use crate::model::{self, fmt_ts};
use crate::store::{self, is_alive, Store};
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
    match crate::sign::load_signing_key() {
        Ok(Some(key)) => crate::sign::sign_intent(&key, from, to, body),
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

pub fn run(
    store: &dyn Store,
    me_default: Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: Vec<StoreSource>,
    pull: PullConsent,
) -> Result<()> {
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
        if let Some(resp) = handle(store, &me_default, nudge_template, &extra_dbs, &pull, &req) {
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

#[allow(clippy::too_many_arguments)]
fn handle(
    store: &dyn Store,
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    req: &Value,
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
            match call_tool(
                store,
                me_default,
                nudge_template,
                extra_dbs,
                pull,
                name,
                &args,
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
) -> Result<String, String> {
    match name {
        "weave_send" => tool_send(store, me_default, nudge_template, args),
        "weave_notify" => tool_notify(store, me_default, nudge_template, args),
        "weave_delivery" => tool_delivery(store, args),
        "weave_outbox" => tool_outbox(store, args),
        "weave_inbox" => tool_inbox(store, me_default, pull, args),
        "weave_history" => tool_history(store, me_default, args),
        "weave_sessions" => tool_sessions(store, me_default, extra_dbs, args),
        "weave_clear" => tool_clear(store, me_default, args),
        "weave_peers" => tool_peers(store, me_default, extra_dbs, args),
        "weave_scan" => tool_scan(store, me_default, extra_dbs, args),
        "weave_reply" => tool_reply(store, me_default, nudge_template, args),
        "weave_thread" => tool_thread(store, args),
        "weave_receipts" => tool_receipts(store, args),
        "weave_doctor" => tool_doctor(store, extra_dbs),
        "weave_whoami" => tool_whoami(store, me_default),
        "weave_attach" => tool_attach(store, me_default, args),
        "weave_set_description" => tool_set_description(store, me_default, args),
        "weave_set_turn_state" => tool_set_turn_state(store, me_default, args),
        "weave_connect" => tool_connect(store, args),
        "weave_ask" => tool_ask(store, me_default, nudge_template, args),
        "weave_answer" => tool_answer(store, me_default, nudge_template, args),
        "weave_ack" => tool_ack(store, me_default, args),
        "weave_asks" => tool_asks(store, me_default, args),
        "weave_ask_get" => tool_ask_get(store, args),
        "weave_ask_many" => tool_ask_many(store, me_default, nudge_template, args),
        "weave_ask_many_result" => tool_ask_many_result(store, args),
        "weave_job_create" => tool_job_create(store, me_default, args),
        "weave_job_list" => tool_job_list(store, args),
        // `show` is the canonical detail view; `status` is its alias (repowire parity).
        "weave_job_show" | "weave_job_status" => tool_job_status(store, args),
        "weave_job_claim" => tool_job_claim(store, me_default, args),
        "weave_job_update" => tool_job_update(store, args),
        "weave_job_result" => tool_job_result(store, args),
        "weave_job_cancel" => tool_job_cancel(store, me_default, args),
        "weave_claim_orchestrator" => tool_claim_orchestrator(store, me_default, args),
        "weave_orchestrator_status" => tool_orchestrator_status(store, args),
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
) -> Result<String, String> {
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
        let sig = sign_intent_if_keyed(&from, to, body);
        let id = store
            .enqueue_intent(to, to_host, &from, subject, body, &sig)
            .map_err(e)?;
        return Ok(format!(
            "Queued intent #{id} from '{from}' for '{to}' @ {store_path} (delivered on their next drain)."
        ));
    }

    let mid = store.send(&from, to, subject, body).map_err(e)?;
    let dest = if model::is_broadcast(to) {
        "broadcast"
    } else {
        to
    };
    let mut out = format!("Sent message #{mid} from '{from}' to '{dest}'.");

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
            let target = Target::from_peer(&peer);
            // Record the post-inject stage AFTER the inject attempt (no store→inject
            // edge — the store records the outcome we pass it).
            let (stage, outcome) = if target.injectable() {
                let (nudge, mode) = build_nudge(nudge_template, &from, body);
                match inject::inject_mode(&target, &nudge, mode) {
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
    Ok(out)
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
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required.")?;
    let subject = bound_subject(args.get("subject").and_then(|v| v.as_str()))?;

    // Persist via the EXISTING send path (no new persistence — notify is a normal
    // stored message; "no reply" is a caller-intent label, not a schema distinction).
    // `store.send` enforces MAX_BODY via check_body, so an oversized body is a clean
    // error (never a panic / partial persist).
    let mid = store
        .send(&from, &to, subject.as_deref(), body)
        .map_err(e)?;

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
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &to, body);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        mid,
        model::DeliveryRefKind::Notify,
        &to,
        stage,
        outcome,
    );

    Ok(format!(
        "Notified '{to}' (#{mid}, no reply expected). {} [{verdict}]",
        verdict_sentence(verdict, &to)
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
                nudge_pulled(store, pull, &me, &p.committed_sources);
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
/// [`inject::Nudge::Nudge`] into THIS session's OWN registered pane (never a
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
    if !target.injectable() || !inject::target_alive(&target) {
        return;
    }
    match inject::inject_mode(&target, "", inject::Nudge::Nudge) {
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
    let local_peers: std::collections::HashMap<String, crate::model::Peer> = store
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
            crate::model::circle_or_default(c) == target
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
        return Some(crate::model::circle_or_default(c).to_string());
    }
    if let Some(d) = def.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Ok(Some(p)) = store.get_peer(d.trim()) {
            if crate::model::PeerRole::from_str(&p.role) == Ok(crate::model::PeerRole::Orchestrator)
            {
                return None;
            }
        }
    }
    Some(crate::config::Config::load().circle())
}

fn tool_peers(
    store: &dyn Store,
    def: &Option<String>,
    extra_dbs: &[StoreSource],
    args: &Value,
) -> Result<String, String> {
    // Tier-1 federation: union local peers with read-only extra stores,
    // origin-tagged. Default (no extra stores) ⇒ the local listing unchanged.
    let mut views = store::federated_peers(store, extra_dbs).map_err(e)?;
    // P4 circle scope (caller-side filter; federation composes).
    if let Some(target) = resolve_mcp_circle(store, def, args).as_deref() {
        views.retain(|v| crate::model::circle_or_default(&v.peer.circle) == target);
    }
    if views.is_empty() {
        return Ok("No peers registered yet. Sessions register via `weave hook session`.".into());
    }
    // Host-aware liveness reason per peer (A2 vocabulary, display-only); mirrors
    // `weave scan` / `weave_scan`. Never a cross-machine probe; secret-free.
    let this_host = crate::config::this_host();
    let now_ts = crate::model::now();
    let mut out = format!("Registered peers ({}):", views.len());
    for v in views {
        let p = &v.peer;
        let inj = if inject::Target::from_peer(p).injectable() {
            "injectable"
        } else {
            "no-inject"
        };
        let liveness = store
            .peer_liveness(p)
            .unwrap_or_else(|_| store::liveness_for(p, &this_host, now_ts));
        let presence = if matches!(liveness, store::Liveness::Stale) {
            "offline"
        } else {
            "online"
        };
        let reason = match liveness {
            store::Liveness::AliveLocal if p.pid.is_some() => "alive (local, pid)",
            store::Liveness::AliveLocal => "alive (local, ttl)",
            store::Liveness::AliveRemote => "alive (remote, ttl)",
            store::Liveness::Stale => "stale",
        };
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
            "\n  • {}{remote_marker} [{presence}] [{reason}]{ts_marker} [{}] {} ({inj}){tags}{desc} seen {}{via}",
            p.name,
            p.mux,
            if p.target.is_empty() { "-" } else { &p.target },
            fmt_ts(p.last_seen)
        ));
    }
    Ok(out)
}

/// Render a peer's git session tags for an MCP listing, e.g. ` {weave@feat/x
/// #my-wt}`, omitting empty fields and the whole group for a non-git session.
/// Mirrors the CLI `fmt_peer_tags`. Pure formatting.
fn fmt_peer_tags(p: &crate::model::Peer) -> String {
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
fn fmt_turn_state(p: &crate::model::Peer) -> String {
    match crate::model::TurnState::from_str(&p.turn_state) {
        Ok(crate::model::TurnState::Working) => " [working]".to_string(),
        Ok(crate::model::TurnState::AwaitingInput) => " [awaiting-input]".to_string(),
        Ok(crate::model::TurnState::PendingFirstTurn) => " [pending]".to_string(),
        _ => String::new(),
    }
}

/// Compact description suffix for an MCP listing (P5), e.g. ` "reviewing PR #23"`.
/// An empty (unset/TTL-expired) description renders nothing. The Peer is expected to
/// carry the read-time-TTL'd view from the store. Pure formatting.
fn fmt_description(p: &crate::model::Peer) -> String {
    if p.description.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", p.description)
    }
}

/// Capture the git session tags for the MCP server's cwd (best-effort, total).
/// MCP has no payload `cwd`, so this uses the process `current_dir()`. A non-git
/// cwd or any failure yields empty tags.
fn git_tags_here() -> crate::git::WorktreeTags {
    match std::env::current_dir() {
        Ok(p) => crate::git::capture_worktree_tags(&p),
        Err(_) => crate::git::WorktreeTags::default(),
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
) -> Result<String, String> {
    // Self-refresh (owner-only-writes), best-effort: a failure is noted to STDERR
    // and never aborts the read.
    if let Some(me) = def.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Ok(me) = bound_ident("me", me) {
            let t = inject::detect_target();
            let tags = git_tags_here();
            if let Err(err) = store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                None,
                Some(std::process::id() as i64),
                &crate::config::this_host(),
                &tags.repo,
                &tags.branch,
                &tags.worktree_id,
                &crate::config::Config::load().circle(),
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
        views.retain(|v| crate::model::circle_or_default(&v.peer.circle) == target);
    }
    if views.is_empty() {
        return Ok("No peers match the scan.".into());
    }
    // Host-aware liveness reason per row (pure A2 reinterpretation of the
    // read-only federated rows; never a cross-machine probe). Mirrors `weave scan`.
    let this_host = crate::config::this_host();
    let now_ts = crate::model::now();
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
            "\n  • {}{remote_marker} [{reason}]{ts_marker} repo={} branch={} worktree={} mux={} pane={} host={}{desc}{via}",
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
        crate::model::ClaimOutcome::Claimed { circle, demoted } => {
            let mut out = format!("claimed role=orchestrator for '{me}' in circle '{circle}'");
            if !demoted.is_empty() {
                out.push_str(&format!(" (demoted: {})", demoted.join(", ")));
            }
            Ok(out)
        }
        crate::model::ClaimOutcome::Refused { circle, holder } => Ok(format!(
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
        .unwrap_or_else(|| crate::config::Config::load().circle());
    let st = store.orchestrator_status(Some(&circle)).map_err(e)?;
    match st.holder {
        Some(h) if st.present => Ok(format!(
            "orchestrator present in circle '{}': '{}' (online)",
            st.circle, h.name
        )),
        _ => Ok(format!("no live orchestrator in circle '{}'", st.circle)),
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
) -> Result<String, String> {
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
    // injectable peer. The recipient is exactly what `reply_target` derived from
    // the parent (no need to re-scan the whole thread, which also capped at
    // MAX_LIMIT and could miss deep threads).
    if let Ok((to, _subject)) = store.reply_target(&from, in_reply_to) {
        if !model::is_broadcast(&to) {
            if let Ok(Some(peer)) = store.get_peer(&to) {
                let target = Target::from_peer(&peer);
                if target.injectable() {
                    let (nudge, mode) = build_nudge(nudge_template, &from, body);
                    match inject::inject_mode(&target, &nudge, mode) {
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
fn tool_doctor(store: &dyn Store, extra_dbs: &[StoreSource]) -> Result<String, String> {
    let target = inject::detect_target();
    // Tier-1 federation: report the union peer count (local + read-only extras).
    let views = store::federated_peers(store, extra_dbs).map_err(e)?;
    let total_peers = views.len();
    let online = views
        .iter()
        .filter(|v| match store.peer_liveness(&v.peer) {
            Ok(l) => !matches!(l, store::Liveness::Stale),
            Err(_) => is_alive(&v.peer),
        })
        .count();
    // Host-aware liveness breakdown over the peer set (A2 vocabulary, display-only),
    // mirroring `weave doctor`. Deterministic given this_host/now; secret-free.
    let this_host = crate::config::this_host();
    let now_ts = crate::model::now();
    let mut peers_alive_local = 0usize;
    let mut peers_alive_remote = 0usize;
    let mut peers_stale = 0usize;
    for v in &views {
        let liveness = store
            .peer_liveness(&v.peer)
            .unwrap_or_else(|_| store::liveness_for(&v.peer, &this_host, now_ts));
        match liveness {
            store::Liveness::AliveLocal => peers_alive_local += 1,
            store::Liveness::AliveRemote => peers_alive_remote += 1,
            store::Liveness::Stale => peers_stale += 1,
        }
    }
    let (fed_ok, fed_skipped) = store::federation_status(extra_dbs);
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
        "\n  peers:          {total_peers} ({online} online)"
    ));
    out.push_str(&format!(
        "\n  liveness:       {peers_alive_local} local-alive, {peers_alive_remote} remote-alive, {peers_stale} stale"
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
            let tiers = crate::config::Config::load().peer_db_remote_token_tiers();
            let per_source = tiers
                .iter()
                .filter(|t| **t == crate::config::PullTokenTier::PerSourceLabel)
                .count();
            let shared = tiers
                .iter()
                .filter(|t| **t == crate::config::PullTokenTier::Shared)
                .count();
            let none = tiers
                .iter()
                .filter(|t| **t == crate::config::PullTokenTier::None)
                .count();
            out.push_str(&format!(
                "\n  remote tokens:  {per_source} per-source, {shared} shared, {none} none"
            ));
            // Token-FREE per-source TIMEOUT-tier observability, parity with the CLI
            // `weave doctor`. Only aggregate tier counts + an effective ms range; never
            // a token byte. The result string is the JSON-RPC tool RESULT (stdout
            // frame); all skip/timeout diagnostics stay on stderr.
            let timeout_tiers = crate::config::Config::load().peer_db_remote_timeout_tiers();
            let t_per_source = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == crate::config::PullTimeoutTier::PerSourceLabel)
                .count();
            let t_global = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == crate::config::PullTimeoutTier::Global)
                .count();
            let t_default = timeout_tiers
                .iter()
                .filter(|(_, t)| *t == crate::config::PullTimeoutTier::Default)
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
    let ph = crate::config::Config::load().federation_health().pull_from;
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
        let cfg = crate::config::Config::load();
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
                .filter(|(_, pk)| {
                    revoked
                        .iter()
                        .any(|e| crate::sign::fingerprint_matches(e, pk))
                })
                .count();
            out.push_str(&format!(
                "\n  revoked keys:   {hit} registered key(s) currently revoked"
            ));
        }
        if let Ok(events) = store.count_revocations() {
            out.push_str(&format!("\n  revocation log: {events} event(s) recorded"));
        }
        let local_fp = crate::sign::local_public_key()
            .ok()
            .flatten()
            .and_then(|pk| crate::sign::fingerprint(&pk))
            .unwrap_or_else(|| "none".to_string());
        out.push_str(&format!("\n  my fingerprint: {local_fp}"));
    }
    out.push_str("\n  (db/config paths: run `weave doctor` on the CLI)");
    // FR6: warn when the resolved store is NOT the well-known XDG default — the most
    // common "why can't I see the other session's peers" cause is a mismatched
    // WEAVE_DB. Compare against the same default `Config::db_path` derives from.
    let db = crate::config::Config::load().db_path();
    let db_default = crate::config::default_db_path();
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
    // P4: report the caller's resolved circle (config/$WEAVE_CIRCLE) and its
    // current registered role, so a caller can confirm its visibility scope. P5:
    // also surface the caller's own turn_state + description (read-time-TTL'd by the
    // store). One peer-row lookup feeds role/turn_state/description; a missing row
    // falls back to the defaults.
    let circle = crate::config::Config::load().circle();
    let me_row = match def {
        Some(d) if !d.trim().is_empty() => store.get_peer(d.trim()).ok().flatten(),
        _ => None,
    };
    let role = me_row
        .as_ref()
        .and_then(|p| crate::model::PeerRole::from_str(&p.role).ok())
        .unwrap_or(crate::model::PeerRole::Peer)
        .as_str()
        .to_string();
    // whoami is a verbose self-report (noise is fine), so turn_state/description are
    // ALWAYS shown — `-` when unset (Unknown / empty / TTL-expired).
    let turn_state = me_row
        .as_ref()
        .and_then(|p| crate::model::TurnState::from_str(&p.turn_state).ok())
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
fn tool_attach(store: &dyn Store, def: &Option<String>, args: &Value) -> Result<String, String> {
    // Resolve + validate the caller's own identity (this is the row key).
    let me = ident(args, "me", def)?;
    let t = inject::detect_target();
    // A detected mux must carry a structurally valid pane id, or we refuse to
    // persist a poisoned, un-injectable registration. A legitimate mux=none has an
    // empty id and is allowed (store-only delivery).
    if t.injectable() && !inject::id_valid(t.mux, &t.id) {
        return Err(format!(
            "refusing to attach: captured target {:?} is not a valid {} target.",
            t.id,
            t.mux.as_str()
        ));
    }
    // Capture the MCP server process's PID + host so the adopted peer reflects
    // real liveness (this is the agent's own process), plus the git session tags
    // derived from the server's cwd (best-effort; a git failure ⇒ empty tags).
    let tags = git_tags_here();
    store
        .register_peer_full(
            &me,
            t.mux.as_str(),
            &t.id,
            &t.socket,
            None,
            Some(std::process::id() as i64),
            &crate::config::this_host(),
            &tags.repo,
            &tags.branch,
            &tags.worktree_id,
            &crate::config::Config::load().circle(),
        )
        .map_err(e)?;
    let tgt = if t.id.is_empty() { "-" } else { &t.id };
    let inj = if t.injectable() {
        "injectable"
    } else {
        "no-inject"
    };
    Ok(format!(
        "Attached '{me}' to the store [{}] {tgt} ({inj}). It is now visible to other sessions.",
        t.mux.as_str()
    ))
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

/// Connect handshake: capability-probe `peer` before sending. Reports a structured
/// verdict and degrades gracefully — a registered-but-not-alive or non-injectable
/// peer is NOT an error (`isError=false`); its messages still arrive via the store
/// on its next turn. Only a non-existent peer is an error.
fn tool_connect(store: &dyn Store, args: &Value) -> Result<String, String> {
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
    let msg = match inject::capability(&target) {
        inject::Capability::Live => format!(
            "Peer '{to}' is live [{}] {} — a live nudge can be delivered now.",
            target.mux.as_str(),
            target.id
        ),
        inject::Capability::RegisteredNotAlive => format!(
            "Peer '{to}' is registered but not alive [{}] {} — delivery will be queued; \
             recipient drains on next turn.",
            target.mux.as_str(),
            target.id
        ),
        inject::Capability::NotInjectable => format!(
            "Peer '{to}' is not injectable (mux=none) — delivery will be queued; \
             recipient drains on next turn."
        ),
    };
    Ok(msg)
}

/// Fire the caller-side live nudge for an ask/answer and compute the HONEST
/// delivery verdict, reusing the EXISTING injector return (no new spawn path, no
/// `store → inject` edge). This is the exact seam `tool_send` uses, lifted into a
/// helper so ask + answer surface the same normalized vocabulary:
///   * `inject_mode` returned `Ok(true)` (a nudge was actually injected) ⇒
///     `transport_delivered`;
///   * a registered-but-not-alive / `Ok(false)` / `Err` peer ⇒ `queued_next_turn`
///     (still succeeds; arrives on the recipient's next drain);
///   * `mux=none` / no peer row ⇒ `recipient_not_injectable`.
///
/// Advisory only: a queued / not-injectable delivery is NEVER an error.
fn ask_delivery_verdict(
    store: &dyn Store,
    nudge_template: Option<&str>,
    from: &str,
    to: &str,
    body: &str,
) -> &'static str {
    let Ok(Some(peer)) = store.get_peer(to) else {
        return "recipient_not_injectable";
    };
    let target = Target::from_peer(&peer);
    match inject::capability(&target) {
        inject::Capability::NotInjectable => "recipient_not_injectable",
        // Injectable (live or registered): fire the same paste-safe nudge tool_send
        // does and report whether it actually landed.
        _ => {
            let (nudge, mode) = build_nudge(nudge_template, from, body);
            match inject::inject_mode(&target, &nudge, mode) {
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
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required (the question).")?;
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
    let (cid, qid) = store
        .ask(&from, &to, subject.as_deref(), body, reply_to)
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
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &to, body);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(store, qid, model::DeliveryRefKind::Ask, &to, stage, outcome);
    Ok(format!(
        "Opened ask {cid} from '{from}' to '{to}'. {}",
        verdict_sentence(verdict, &to)
    ))
}

/// `weave_answer`: reply along a tracked thread back to the asker. Accepts either
/// `correlation_id` or an `in_reply_to` message id (resolved to its owning ask).
fn tool_answer(
    store: &dyn Store,
    def: &Option<String>,
    nudge_template: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let from = ident(args, "from", def)?;
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("'body' is required (the answer).")?;
    let cid = resolve_correlation_id(store, args)?;
    let ask = store
        .get_ask(&cid)
        .map_err(e)?
        .ok_or_else(|| format!("No tracked ask '{cid}'."))?;
    let asker = ask.asker.clone();
    let ans_id = store.answer(&from, &cid, body).map_err(e)?;
    record_delivery_best_effort(
        store,
        ans_id,
        model::DeliveryRefKind::Ask,
        &asker,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let verdict = ask_delivery_verdict(store, nudge_template, &from, &asker, body);
    let (stage, outcome) = verdict_to_stage(verdict);
    record_delivery_best_effort(
        store,
        ans_id,
        model::DeliveryRefKind::Ask,
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
                let verdict = ask_delivery_verdict(store, nudge_template, &from, peer, body);
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

fn tools() -> Value {
    json!([
        {
            "name": "weave_send",
            "description": "Send a message to another agent session. 'to' = a session name, or 'all'/'*' to broadcast. If the recipient is a registered injectable peer (tmux/zellij), a live nudge is pushed into its pane immediately; otherwise it arrives on the recipient's next turn. Cross-store (Tier-2): pass 'to_store' = a path to another store to queue the message as an intent in YOUR OWN outbox; the recipient pulls and commits it on its next drain (no foreign write, no broadcast).",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name, or 'all'."},
                "subject":{"type":"string"},
                "body":{"type":"string"},
                "to_store":{"type":"string","description":"Cross-store: path to the recipient's store. Queues a directed intent in your outbox (next-drain delivery); not valid with broadcast."},
                "to_host":{"type":"string","description":"Optional host hint for a cross-store intent (advisory)."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_notify",
            "description": "Fire-and-forget notification to a peer (no reply expected). Persists the message and pushes a live nudge if the recipient is injectable, then returns the HONEST delivery verdict: transport_delivered (nudge landed live) / queued_next_turn (registered or not alive — arrives on next drain) / recipient_not_injectable. An unknown peer is NOT an error — the message still waits in the store. Point-to-point only; use weave_send for broadcast.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"Recipient session name (point-to-point; broadcast is not supported)."},
                "subject":{"type":"string"},
                "body":{"type":"string"}
            },"required":["to","body"]}
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
            "name": "weave_set_description",
            "description": "Set YOUR OWN free-form, self-reported task description (a one-line summary). Surfaces compactly in weave_peers/weave_sessions/weave_scan and to whoami, and ages out after the description TTL (900s). Self-only: only ever updates the caller's own peer row. Oversized text is truncated (never an error); control chars are stripped. Pass an empty string to clear it.",
            "inputSchema": {"type":"object","properties":{
                "description":{"type":"string","description":"One-line task summary (capped + control-stripped; empty clears it)."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["description"]}
        },
        {
            "name": "weave_set_turn_state",
            "description": "Explicitly set YOUR OWN turn-state (P5 rich presence). Normally hook-auto via `weave hook session|prompt|stop|notification`; this is the manual override. Self-only. An invalid state is an error.",
            "inputSchema": {"type":"object","properties":{
                "state":{"type":"string","enum":["pending_first_turn","working","awaiting_input","idle"],"description":"The turn-state to set."},
                "me":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."}
            },"required":["state"]}
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
            "description": "Open a correlation-TRACKED request to a peer and return its correlation_id immediately (NON-blocking — not a synchronous RPC). The question is delivered like a normal message (live nudge if injectable, else next-turn) and the result reports the honest delivery verdict (transport_delivered / queued_next_turn / recipient_not_injectable). Point-to-point only (no broadcast). Optional reply_to chains a new ask off a prior one, closing the prior thread.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (or omit to use WEAVE_SESSION)."},
                "to":{"type":"string","description":"The peer session name to ask."},
                "body":{"type":"string","description":"The question."},
                "subject":{"type":"string"},
                "reply_to":{"type":"string","description":"Optional prior correlation_id this ask chains/closes."}
            },"required":["to","body"]}
        },
        {
            "name": "weave_answer",
            "description": "Answer a tracked ask, replying back along the thread to whoever opened it and transitioning the ask open->answered. Reference the thread by correlation_id OR by an in_reply_to message id (resolved to its owning ask). Reports the honest delivery verdict to the asker. Errors on an unknown thread, an already-acked thread, or a responder who is not the askee.",
            "inputSchema": {"type":"object","properties":{
                "from":{"type":"string","description":"Your session name (must be the askee)."},
                "correlation_id":{"type":"string","description":"The ask's correlation_id."},
                "in_reply_to":{"type":"integer","description":"Alternatively, a message id belonging to the ask."},
                "body":{"type":"string","description":"The answer."}
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
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Unknown token degrades safely to queued (the message is in the store).
        assert_eq!(
            verdict_to_stage("anything_else"),
            (DeliveryStage::Queued, DeliveryOutcome::Ok)
        );
    }
}
