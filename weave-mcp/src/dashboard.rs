//! WL-048 / ADR-0004: pure render layer for the read-only human web dashboard.
//!
//! Everything here is **socket-free and pure**: it receives data already fetched
//! through the `Store` trait (the caller in [`crate::http`] does the I/O) and turns
//! it into HTML / SSE frames. That purity is what makes the dashboard unit-testable
//! with no listener and no DB, and it keeps the layer DAG intact (this module reads
//! no DB and opens no socket).
//!
//! ## XSS — the single biggest correctness risk
//!
//! EVERY Store-derived string rendered into the HTML (peer names, message bodies,
//! subjects, job titles/descriptions, lease holders, schedule bodies, repo/branch
//! tags) MUST pass through [`html_escape`]. There is exactly ONE escape helper and
//! every interpolation of untrusted text goes through it — never `format!("…{body}…")`
//! of raw Store text. The XSS regression test below locks this in.

use weave_core::export::html_escape;
use weave_core::model::{
    fmt_ts, peer_session_id, Ask, AskState, Job, Lease, Message, Peer, Schedule,
};

const DASHBOARD_SCRIPT: &str = r#"<script>
(() => {
  try {
    const p = new URLSearchParams(location.search);
    const t = p.get('token') || p.get('access_token');
    if (t) document.cookie = 'weave_dashboard_token=' + encodeURIComponent(t) + '; SameSite=Lax; path=/';
  } catch (_) {}
  let lastSince = 0;
  const remember = (payload) => {
    const ids = (payload.events || []).map((e) => String(e.id || '').replace(/^msg_/, '')).map(Number).filter(Number.isFinite);
    if (ids.length) lastSince = Math.max(lastSince, ...ids);
    if (typeof payload.next_since === 'number') lastSince = Math.max(lastSince, payload.next_since);
  };
  const recover = async () => {
    try {
      const r = await fetch('/events?since=' + encodeURIComponent(String(lastSince)), { credentials: 'same-origin' });
      if (r.ok) remember(await r.json());
    } catch (_) {}
  };
  recover();
  if ('EventSource' in window) {
    const es = new EventSource('/events/stream', { withCredentials: true });
    es.onmessage = (ev) => {
      if (typeof ev.data === 'string' && ev.data.startsWith('<!doctype html>')) {
        document.open();
        document.write(ev.data);
        document.close();
      }
    };
    es.onerror = () => { recover(); };
  }
})();
</script>"#;

/// Build a single Server-Sent Events frame. Per the SSE spec each line of `data`
/// is emitted as its own `data:` field and the event is terminated by a blank
/// line. CR is stripped (the spec treats CR/LF/CRLF as line separators; we emit
/// clean LF only). The result always ends with the terminating `\n\n`.
pub fn sse_event(data: &str) -> String {
    let mut out = String::with_capacity(data.len() + 8);
    for line in data.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// An SSE keep-alive comment line. A `:`-prefixed line is ignored by the client
/// but keeps the connection (and any intermediary) from timing out.
pub fn sse_keepalive() -> &'static str {
    ": ping\n\n"
}

/// HTTP route classification for the surfaces extension. The MCP JSON-RPC POST
/// path is unchanged; the dashboard only adds two read-only GET routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /` — the server-rendered HTML dashboard page.
    Page,
    /// `GET /events` — the long-lived SSE stream.
    Events,
    /// `GET /api/snapshot` — browser-friendly JSON snapshot for the dashboard.
    SnapshotJson,
    /// `GET /peers` — repowire-dashboard compatibility peer roster JSON.
    PeersJson,
    /// `GET /events` when JSON is requested by a fetch client.
    EventsJson,
    /// `GET /jobs?view=summary` — repowire-dashboard compatibility job summary.
    JobsJson,
    /// `GET /asks/pending` — repowire-dashboard compatibility pending question list.
    AsksPendingJson,
    /// `GET /settings` / `/api/settings` — token-free dashboard config posture.
    SettingsJson,
    /// `GET /health` — read-only dashboard API health.
    HealthJson,
    /// `POST /` — the existing MCP JSON-RPC surface (left untouched).
    JsonRpc,
    /// Anything else — `404`.
    NotFound,
}

/// Classify a request from its method + path. Pure; the caller (http.rs) maps the
/// route to the right writer. `POST /` stays [`Route::JsonRpc`] so the surfaces
/// extension provably does not alter the MCP path.
pub fn route(method: &str, path: &str) -> Route {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    match (method, path) {
        ("GET", "/") => Route::Page,
        ("GET", "/events") if query_has_key(query, "since") => Route::EventsJson,
        ("GET", "/events") => Route::Events,
        ("GET", "/events/stream") => Route::Events,
        ("GET", "/api/events") => Route::EventsJson,
        ("GET", "/api/snapshot") => Route::SnapshotJson,
        ("GET", "/peers") => Route::PeersJson,
        ("GET", "/jobs") => Route::JobsJson,
        ("GET", "/asks/pending") => Route::AsksPendingJson,
        ("GET", "/settings") => Route::SettingsJson,
        ("GET", "/api/settings") => Route::SettingsJson,
        ("GET", "/health") => Route::HealthJson,
        ("POST", "/") => Route::JsonRpc,
        _ => Route::NotFound,
    }
}

fn query_has_key(query: &str, wanted: &str) -> bool {
    query.split('&').any(|pair| {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        key == wanted
    })
}

/// A read-only snapshot of mesh state for one dashboard render. Composed by the
/// caller from existing `Store` reads plus token-free runtime config posture —
/// this module never opens the DB or reads env/config directly.
#[derive(Debug, Default, Clone)]
pub struct DashboardSnapshot {
    pub peers: Vec<Peer>,
    pub messages: Vec<Message>,
    pub jobs: Vec<Job>,
    pub asks: Vec<Ask>,
    pub leases: Vec<Lease>,
    pub schedules: Vec<Schedule>,
    pub settings: DashboardSettings,
}

/// Token-free dashboard/runtime posture. Booleans and counts are fine; secret
/// values (bearer tokens, bot tokens, libSQL auth, pull tokens, proxy credentials)
/// never appear here.
#[derive(Debug, Default, Clone)]
pub struct DashboardSettings {
    pub circle: String,
    pub write_enabled: bool,
    pub spawn_allowed_dirs: Vec<String>,
    pub peer_db_count: usize,
    pub pull_from_count: usize,
    pub inject_pulled: bool,
    pub allow_inject_from_count: Option<usize>,
    pub bridge_identity: String,
    pub telegram_configured: bool,
    pub slack_configured: bool,
    pub pretooluse_approver_configured: bool,
    pub pretooluse_timeout_secs: i64,
    pub obscura_allow_ops: Vec<String>,
    pub obscura_allow_domains: Vec<String>,
    pub obscura_allow_internal: bool,
}

/// Presence TTL (seconds): a peer last seen within this window of `now` renders as
/// "live", else "idle". Matches the recency heuristic used elsewhere; purely a
/// display label here.
const PRESENCE_TTL_SECS: i64 = 90;

/// Render the full dashboard HTML page. **Pure**: deterministic given
/// `(snapshot, now, host)` — no clock reads, no env reads (the text-dashboard
/// convention). Every Store-derived string is passed through [`html_escape`].
pub fn render_dashboard(snap: &DashboardSnapshot, now: i64, host: &str) -> String {
    let mut b = String::with_capacity(4096);
    b.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    b.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    b.push_str("<title>weave dashboard</title>");
    b.push_str("<style>");
    b.push_str(
        ":root{color-scheme:dark;--bg:#070b12;--panel:#0d1422;--panel2:#101b2d;--line:#243149;\
         --text:#d8e5f2;--muted:#8ea0b8;--accent:#62a8ff;--ok:#3fb950;--warn:#d29922;--bad:#ff7b72}\
         *{box-sizing:border-box}body{font-family:Inter,ui-sans-serif,system-ui,sans-serif;margin:0;background:radial-gradient(circle at 10% 0%,#12233a 0,#070b12 36rem);color:var(--text)}\
         .shell{min-height:100vh;display:grid;grid-template-rows:auto 1fr}.top{height:56px;display:flex;align-items:center;justify-content:space-between;padding:0 1rem;border-bottom:1px solid var(--line);background:rgba(7,11,18,.85);backdrop-filter:blur(12px);position:sticky;top:0;z-index:1}\
         .brand{font-weight:800;letter-spacing:.02em}.brand code{color:var(--accent)}.pill{border:1px solid var(--line);background:var(--panel);border-radius:999px;padding:.25rem .55rem;color:var(--muted);font-size:.8rem}\
         .grid{display:grid;grid-template-columns:320px minmax(0,1fr) 360px;gap:1rem;padding:1rem}.main-col{display:flex;flex-direction:column;gap:1rem}.panel{background:linear-gradient(180deg,var(--panel),#0a101b);border:1px solid var(--line);border-radius:16px;box-shadow:0 10px 30px #0006;overflow:hidden}\
         .panel h2{font-size:.78rem;text-transform:uppercase;letter-spacing:.12em;color:var(--muted);margin:0;padding:.85rem 1rem;border-bottom:1px solid var(--line)}\
         .panel-body{padding:.8rem 1rem}.stats{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:.75rem;margin-bottom:1rem}.stat{background:var(--panel2);border:1px solid var(--line);border-radius:14px;padding:.8rem}.stat strong{display:block;font-size:1.4rem}.stat span{color:var(--muted);font-size:.78rem}\
         table{border-collapse:collapse;width:100%;font-size:.82rem}td,th{text-align:left;padding:.35rem .45rem;border-bottom:1px solid #1c2739;vertical-align:top}\
         th{color:var(--muted);font-weight:650}.live{color:var(--ok)}.idle{color:var(--muted)}.busy{color:var(--warn)}.empty,.muted{color:var(--muted);font-style:italic}code{color:#79c0ff}.feed{display:flex;flex-direction:column;gap:.6rem}.event{padding:.65rem .75rem;border:1px solid var(--line);border-radius:12px;background:#0b1220}.event-meta{color:var(--muted);font-size:.75rem;margin-bottom:.3rem}.event-body{white-space:pre-wrap;overflow-wrap:anywhere}.peer-card{display:grid;grid-template-columns:1fr auto;gap:.25rem .5rem;border-bottom:1px solid #1c2739;padding:.55rem 0}.peer-card:last-child{border-bottom:0}.peer-name{font-weight:700}.peer-sub{color:var(--muted);font-size:.78rem}.detail-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.55rem}.detail-item{border:1px solid var(--line);background:var(--panel2);border-radius:12px;padding:.55rem}.detail-item span{display:block;color:var(--muted);font-size:.72rem;text-transform:uppercase;letter-spacing:.08em}.ask-card,.job-card{border:1px solid var(--line);background:#0b1220;border-radius:12px;padding:.65rem .75rem;margin-bottom:.55rem}.ask-card strong,.job-card strong{display:block}.ask-meta,.job-meta{color:var(--muted);font-size:.75rem;margin-top:.25rem}.action-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:.75rem}.action-form{border:1px solid var(--line);background:#0b1220;border-radius:12px;padding:.75rem}.action-form label{display:block;color:var(--muted);font-size:.72rem;margin-top:.45rem}.action-form input,.action-form textarea{width:100%;margin-top:.18rem;border:1px solid var(--line);border-radius:8px;background:#060a11;color:var(--text);padding:.42rem}.action-form textarea{min-height:4rem;resize:vertical}.action-form button,.inline-form button{margin-top:.55rem;border:1px solid #2f81f7;background:#1f6feb;color:white;border-radius:999px;padding:.42rem .75rem;cursor:pointer}.choice-row{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.55rem}.inline-form{display:flex;gap:.35rem;align-items:center;margin-top:.55rem}.inline-form input{min-width:0;border:1px solid var(--line);border-radius:8px;background:#060a11;color:var(--text);padding:.35rem}.section{margin-bottom:1rem}@media(max-width:1100px){.grid{grid-template-columns:1fr}.stats{grid-template-columns:repeat(2,minmax(0,1fr))}}",
    );
    b.push_str("</style>");
    b.push_str(DASHBOARD_SCRIPT);
    b.push_str("</head><body>");
    b.push_str(
        "<div class=\"shell\"><header class=\"top\"><div class=\"brand\">weave dashboard <code>",
    );
    b.push_str(&html_escape(host));
    b.push_str("</code></div><div class=\"pill\">repowire-grade Rust surface · ");
    b.push_str(if snap.settings.write_enabled {
        "write-enabled"
    } else {
        "read-only"
    });
    b.push_str(" · ");
    b.push_str(&html_escape(&fmt_ts(now)));
    b.push_str("</div></header><main class=\"grid\">");

    let live = snap
        .peers
        .iter()
        .filter(|p| now - p.last_seen <= PRESENCE_TTL_SECS)
        .count();
    b.push_str("<section class=\"panel\"><h2>Peer roster</h2><div class=\"panel-body\">");
    b.push_str(&format!(
        "<div class=\"stats\"><div class=\"stat\"><strong>{}</strong><span>peers</span></div><div class=\"stat\"><strong>{}</strong><span>live</span></div><div class=\"stat\"><strong>{}</strong><span>jobs</span></div><div class=\"stat\"><strong>{}</strong><span>open asks</span></div></div>",
        snap.peers.len(),
        live,
        snap.jobs.len(),
        snap.asks.iter().filter(|a| a.state == AskState::Open).count()
    ));
    render_peer_cards(&mut b, snap, now);
    b.push_str("</div></section>");

    b.push_str("<div class=\"main-col\"><section class=\"panel\"><h2>Selected peer</h2><div class=\"panel-body\">");
    render_selected_peer_detail(&mut b, snap);
    b.push_str("</div></section><section class=\"panel\"><h2>Pending questions</h2><div class=\"panel-body\">");
    render_pending_questions(&mut b, snap);
    b.push_str(
        "</div></section><section class=\"panel\"><h2>Selected job</h2><div class=\"panel-body\">",
    );
    render_selected_job_detail(&mut b, snap);
    b.push_str(
        "</div></section><section class=\"panel\"><h2>Actions</h2><div class=\"panel-body\">",
    );
    render_action_forms(&mut b, snap);
    b.push_str(
        "</div></section><section class=\"panel\"><h2>Danger zone</h2><div class=\"panel-body\">",
    );
    render_danger_zone(&mut b, snap);
    b.push_str(
        "</div></section><section class=\"panel\"><h2>Settings</h2><div class=\"panel-body\">",
    );
    render_settings_panel(&mut b, snap);
    b.push_str(
        "</div></section><section class=\"panel\"><h2>Mesh feed</h2><div class=\"panel-body\">",
    );
    render_feed_cards(&mut b, snap);
    b.push_str("</div></section></div>");

    b.push_str("<aside class=\"panel\"><h2>Control plane</h2><div class=\"panel-body\">");

    render_jobs_cards(&mut b, snap);
    render_peers(&mut b, snap, now);
    render_messages(&mut b, snap);
    render_jobs(&mut b, snap);
    render_leases(&mut b, snap);
    render_schedules(&mut b, snap);

    b.push_str("</div></aside></main></div></body></html>");
    b
}

fn render_peer_cards(b: &mut String, snap: &DashboardSnapshot, now: i64) {
    if snap.peers.is_empty() {
        b.push_str("<p class=\"empty\">no sessions</p>");
        return;
    }
    for p in &snap.peers {
        let live = now - p.last_seen <= PRESENCE_TTL_SECS;
        let (cls, label) = if live {
            ("live", "live")
        } else {
            ("idle", "idle")
        };
        b.push_str("<div class=\"peer-card\"><div><div class=\"peer-name\">");
        b.push_str(&html_escape(&p.name));
        b.push_str("</div><div class=\"peer-sub\">");
        b.push_str(&html_escape(&format!(
            "{} · {} · {}",
            p.mux,
            if p.repo.is_empty() { "-" } else { &p.repo },
            fmt_ts(p.last_seen)
        )));
        b.push_str("</div></div><div class=\"");
        b.push_str(cls);
        b.push_str("\">");
        b.push_str(label);
        b.push_str("</div></div>");
    }
}

fn render_feed_cards(b: &mut String, snap: &DashboardSnapshot) {
    if snap.messages.is_empty() {
        b.push_str("<p class=\"empty\">no messages</p>");
        return;
    }
    b.push_str("<div class=\"feed\">");
    for m in snap.messages.iter().take(24) {
        b.push_str("<article class=\"event\"><div class=\"event-meta\">");
        b.push_str(&html_escape(&fmt_ts(m.ts)));
        b.push_str(" · ");
        b.push_str(&html_escape(&m.sender));
        b.push_str(" → ");
        b.push_str(&html_escape(&m.recipient));
        if let Some(subject) = m.subject.as_deref().filter(|s| !s.is_empty()) {
            b.push_str(" · ");
            b.push_str(&html_escape(subject));
        }
        b.push_str("</div><div class=\"event-body\">");
        b.push_str(&html_escape(&m.body));
        b.push_str("</div></article>");
    }
    b.push_str("</div>");
}

fn render_selected_peer_detail(b: &mut String, snap: &DashboardSnapshot) {
    let Some(p) = snap.peers.first() else {
        b.push_str("<p class=\"empty\">select a peer to inspect its session, transcript, MCP context, and controls</p>");
        return;
    };
    b.push_str("<div class=\"detail-grid\">");
    detail_item(b, "name", &p.name);
    detail_item(b, "session", &peer_session_id(p));
    detail_item(b, "role", if p.role.is_empty() { "peer" } else { &p.role });
    detail_item(
        b,
        "turn state",
        if p.turn_state.is_empty() {
            "unknown"
        } else {
            &p.turn_state
        },
    );
    detail_item(b, "cwd", p.cwd.as_deref().unwrap_or(""));
    detail_item(b, "repo", if p.repo.is_empty() { "-" } else { &p.repo });
    detail_item(
        b,
        "branch",
        if p.branch.is_empty() { "-" } else { &p.branch },
    );
    detail_item(
        b,
        "description",
        if p.description.is_empty() {
            "-"
        } else {
            &p.description
        },
    );
    b.push_str("</div><h2>Session controls</h2><div class=\"choice-row\">");
    b.push_str("<form method=\"post\" action=\"/api/turn-state\" class=\"inline-form\"><input type=\"hidden\" name=\"me\" value=\"");
    b.push_str(&html_escape(&p.name));
    b.push_str("\"><input name=\"state\" placeholder=\"working|awaiting_input|idle\" value=\"");
    b.push_str(&html_escape(if p.turn_state.is_empty() {
        "working"
    } else {
        &p.turn_state
    }));
    b.push_str("\"><button type=\"submit\">Set turn state</button></form>");
    b.push_str("<form method=\"post\" action=\"/api/description\" class=\"inline-form\"><input type=\"hidden\" name=\"me\" value=\"");
    b.push_str(&html_escape(&p.name));
    b.push_str("\"><input name=\"description\" placeholder=\"task description\" value=\"");
    b.push_str(&html_escape(&p.description));
    b.push_str("\"><button type=\"submit\">Set description</button></form></div>");
    b.push_str("<h2>Transcript preview</h2>");
    let mut count = 0usize;
    let mut latest_message_id = None;
    b.push_str("<div class=\"feed\">");
    for m in snap
        .messages
        .iter()
        .filter(|m| m.sender == p.name || m.recipient == p.name)
        .take(8)
    {
        if latest_message_id.is_none() {
            latest_message_id = Some(m.id);
        }
        count += 1;
        b.push_str("<article class=\"event\"><div class=\"event-meta\">");
        b.push_str(&html_escape(&fmt_ts(m.ts)));
        b.push_str(" · ");
        b.push_str(&html_escape(&m.sender));
        b.push_str(" → ");
        b.push_str(&html_escape(&m.recipient));
        b.push_str("</div><div class=\"event-body\">");
        b.push_str(&html_escape(&m.body));
        b.push_str("</div></article>");
    }
    b.push_str("</div>");
    if count == 0 {
        b.push_str("<p class=\"empty\">no transcript messages for selected peer</p>");
    } else if let Some(mid) = latest_message_id {
        b.push_str("<form class=\"action-form\" method=\"post\" action=\"/api/reply\"><strong>Reply in transcript</strong>");
        input(b, "from", "from", &p.name);
        input(b, "in_reply_to", "message id", &mid.to_string());
        textarea(b, "body", "reply", "");
        b.push_str("<button type=\"submit\">Reply</button></form>");
    }
}

fn detail_item(b: &mut String, label: &str, value: &str) {
    b.push_str("<div class=\"detail-item\"><span>");
    b.push_str(&html_escape(label));
    b.push_str("</span>");
    b.push_str(&html_escape(value));
    b.push_str("</div>");
}

fn render_selected_job_detail(b: &mut String, snap: &DashboardSnapshot) {
    let Some(j) = snap.jobs.first() else {
        b.push_str("<p class=\"empty\">select a job to inspect state, ownership, progress, result, cancellation, and retry context</p>");
        return;
    };
    b.push_str("<div class=\"detail-grid\">");
    detail_item(b, "id", &j.id);
    detail_item(b, "title", &j.title);
    detail_item(b, "state", j.state.as_str());
    detail_item(b, "kind", &j.kind);
    detail_item(b, "creator", &j.creator);
    detail_item(b, "owner", j.owner.as_deref().unwrap_or("-"));
    detail_item(b, "assignee", j.assignee.as_deref().unwrap_or("-"));
    detail_item(b, "phase", j.phase.as_deref().unwrap_or("-"));
    detail_item(
        b,
        "progress note",
        j.progress_note.as_deref().unwrap_or("-"),
    );
    detail_item(
        b,
        "result summary",
        j.result_summary.as_deref().unwrap_or("-"),
    );
    detail_item(b, "opened", &fmt_ts(j.opened_ts));
    detail_item(b, "updated", &fmt_ts(j.updated_ts));
    detail_item(
        b,
        "deadline",
        &j.deadline_at.map(fmt_ts).unwrap_or_else(|| "-".to_string()),
    );
    detail_item(
        b,
        "expires",
        &j.expires_at.map(fmt_ts).unwrap_or_else(|| "-".to_string()),
    );
    detail_item(
        b,
        "cancel requested",
        if j.cancel_requested { "yes" } else { "no" },
    );
    detail_item(
        b,
        "cancel reason",
        j.cancel_reason.as_deref().unwrap_or("-"),
    );
    b.push_str("</div>");
    if !j.description.is_empty() {
        b.push_str("<h2>Description</h2><div class=\"event-body\">");
        b.push_str(&html_escape(&j.description));
        b.push_str("</div>");
    }
    if let Some(prompt) = j.prompt.as_deref().filter(|s| !s.is_empty()) {
        b.push_str("<h2>Prompt</h2><div class=\"event-body\">");
        b.push_str(&html_escape(prompt));
        b.push_str("</div>");
    }
    render_job_progress_events(b, j);
    b.push_str("<p class=\"muted\">Read API: <code>/jobs/");
    b.push_str(&html_escape(&j.id));
    b.push_str("/status</code> and <code>/jobs/");
    b.push_str(&html_escape(&j.id));
    b.push_str("/result</code>.</p>");
}

fn render_job_progress_events(b: &mut String, j: &Job) {
    b.push_str("<h2>Progress timeline</h2>");
    let events = serde_json::from_str::<serde_json::Value>(&j.progress_events_json)
        .unwrap_or_else(|_| serde_json::json!([]));
    let Some(events) = events.as_array() else {
        b.push_str("<p class=\"empty\">no progress events</p>");
        return;
    };
    if events.is_empty() {
        b.push_str("<p class=\"empty\">no progress events</p>");
        return;
    }
    b.push_str("<div class=\"feed\">");
    for ev in events.iter().rev().take(8) {
        let at = ev.get("at").and_then(|v| v.as_i64()).map(fmt_ts);
        let state = ev.get("state").and_then(|v| v.as_str()).unwrap_or("-");
        let phase = ev.get("phase").and_then(|v| v.as_str()).unwrap_or("-");
        let note = ev.get("note").and_then(|v| v.as_str()).unwrap_or("");
        b.push_str("<article class=\"event\"><div class=\"event-meta\">");
        b.push_str(&html_escape(at.as_deref().unwrap_or("-")));
        b.push_str(" · ");
        b.push_str(&html_escape(state));
        if phase != "-" {
            b.push_str(" · ");
            b.push_str(&html_escape(phase));
        }
        b.push_str("</div><div class=\"event-body\">");
        b.push_str(&html_escape(note));
        b.push_str("</div></article>");
    }
    b.push_str("</div>");
}

fn render_pending_questions(b: &mut String, snap: &DashboardSnapshot) {
    let mut open = snap
        .asks
        .iter()
        .filter(|a| a.state == AskState::Open)
        .peekable();
    if open.peek().is_none() {
        b.push_str("<p class=\"empty\">no pending questions</p>");
        return;
    }
    for a in open.take(12) {
        b.push_str("<article class=\"ask-card\"><strong>");
        b.push_str(&html_escape(a.subject.as_deref().unwrap_or("question")));
        b.push_str("</strong><div class=\"ask-meta\"><code>");
        b.push_str(&html_escape(&a.id));
        b.push_str("</code> · ");
        b.push_str(&html_escape(&a.asker));
        b.push_str(" → ");
        b.push_str(&html_escape(&a.askee));
        b.push_str(" · ");
        b.push_str(&html_escape(a.kind.as_str()));
        b.push_str(" · updated ");
        b.push_str(&html_escape(&fmt_ts(a.updated_ts)));
        b.push_str("</div>");
        if let Some(options) = a.options.as_deref().filter(|s| !s.is_empty()) {
            b.push_str("<div class=\"event-body\">");
            b.push_str(&html_escape(options));
            b.push_str("</div>");
        }
        render_ask_answer_controls(b, a);
        b.push_str("</article>");
    }
}

fn render_ask_answer_controls(b: &mut String, ask: &Ask) {
    match ask.kind {
        weave_core::model::AskKind::Choice => {
            if let Some(options) = ask.options.as_deref() {
                b.push_str("<div class=\"choice-row\">");
                for option in options
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(8)
                {
                    b.push_str("<form method=\"post\" action=\"/api/answer\" class=\"inline-form\"><input type=\"hidden\" name=\"from\" value=\"");
                    b.push_str(&html_escape(&ask.askee));
                    b.push_str("\"><input type=\"hidden\" name=\"correlation_id\" value=\"");
                    b.push_str(&html_escape(&ask.id));
                    b.push_str("\"><input type=\"hidden\" name=\"body\" value=\"");
                    b.push_str(&html_escape(option));
                    b.push_str("\"><button type=\"submit\">");
                    b.push_str(&html_escape(option));
                    b.push_str("</button></form>");
                }
                b.push_str("</div>");
            }
        }
        weave_core::model::AskKind::ToolPermission => {
            b.push_str("<div class=\"choice-row\">");
            for verdict in ["approve", "deny"] {
                b.push_str("<form method=\"post\" action=\"/api/answer\" class=\"inline-form\"><input type=\"hidden\" name=\"from\" value=\"");
                b.push_str(&html_escape(&ask.askee));
                b.push_str("\"><input type=\"hidden\" name=\"correlation_id\" value=\"");
                b.push_str(&html_escape(&ask.id));
                b.push_str("\"><input type=\"hidden\" name=\"body\" value=\"");
                b.push_str(verdict);
                b.push_str("\"><button type=\"submit\">");
                b.push_str(verdict);
                b.push_str("</button></form>");
            }
            b.push_str("</div>");
        }
        weave_core::model::AskKind::FreeText => {
            b.push_str("<form method=\"post\" action=\"/api/answer\" class=\"inline-form\"><input type=\"hidden\" name=\"from\" value=\"");
            b.push_str(&html_escape(&ask.askee));
            b.push_str("\"><input type=\"hidden\" name=\"correlation_id\" value=\"");
            b.push_str(&html_escape(&ask.id));
            b.push_str("\"><input name=\"body\" placeholder=\"answer\"><button type=\"submit\">Answer</button></form>");
        }
    }
}

fn render_jobs_cards(b: &mut String, snap: &DashboardSnapshot) {
    b.push_str("<h2>Job cards</h2>");
    if snap.jobs.is_empty() {
        b.push_str("<p class=\"empty\">no jobs</p>");
        return;
    }
    for j in snap.jobs.iter().take(8) {
        b.push_str("<article class=\"job-card\"><strong>");
        b.push_str(&html_escape(&j.title));
        b.push_str("</strong><div class=\"job-meta\"><code>");
        b.push_str(&html_escape(&j.id));
        b.push_str("</code> · ");
        b.push_str(&html_escape(j.state.as_str()));
        if let Some(assignee) = j.assignee.as_deref() {
            b.push_str(" · assigned ");
            b.push_str(&html_escape(assignee));
        }
        if let Some(phase) = j.phase.as_deref() {
            b.push_str(" · ");
            b.push_str(&html_escape(phase));
        }
        b.push_str("</div>");
        if !j.description.is_empty() {
            b.push_str("<div class=\"event-body\">");
            b.push_str(&html_escape(&j.description));
            b.push_str("</div>");
        }
        if !matches!(
            j.state,
            weave_core::model::JobState::Completed
                | weave_core::model::JobState::Failed
                | weave_core::model::JobState::Cancelled
                | weave_core::model::JobState::Expired
                | weave_core::model::JobState::Unavailable
        ) {
            b.push_str("<form method=\"post\" action=\"/api/job-cancel\" class=\"inline-form\"><input type=\"hidden\" name=\"job_id\" value=\"");
            b.push_str(&html_escape(&j.id));
            b.push_str("\"><input name=\"from\" placeholder=\"from\" value=\"");
            b.push_str(&html_escape(j.owner.as_deref().unwrap_or(&j.creator)));
            b.push_str("\"><input name=\"reason\" placeholder=\"reason\"><button type=\"submit\">Cancel</button></form>");
        } else {
            b.push_str("<form method=\"post\" action=\"/api/job-create\" class=\"inline-form\"><input type=\"hidden\" name=\"creator\" value=\"");
            b.push_str(&html_escape(&j.creator));
            b.push_str("\"><input type=\"hidden\" name=\"title\" value=\"Retry: ");
            b.push_str(&html_escape(&j.title));
            b.push_str("\"><input type=\"hidden\" name=\"description\" value=\"");
            b.push_str(&html_escape(&j.description));
            b.push_str("\"><input type=\"hidden\" name=\"kind\" value=\"");
            b.push_str(&html_escape(&j.kind));
            b.push_str("\"><button type=\"submit\">Recreate</button></form>");
        }
        b.push_str("</article>");
    }
}

fn render_action_forms(b: &mut String, snap: &DashboardSnapshot) {
    let from = snap.peers.first().map(|p| p.name.as_str()).unwrap_or("");
    let to = snap.peers.get(1).map(|p| p.name.as_str()).unwrap_or("");
    let pending = snap.asks.iter().find(|a| a.state == AskState::Open);
    b.push_str("<p class=\"muted\">Forms post to the bearer-gated dashboard action API and are routed through the same JSON-RPC dispatch path as MCP/CLI. Start with <code>weave dashboard --write</code> to enable them.</p>");
    b.push_str("<div class=\"action-grid\">");
    b.push_str("<form class=\"action-form\" method=\"post\" action=\"/api/notify\"><strong>Notify</strong>");
    input(b, "from", "from", from);
    input(b, "to", "to", to);
    input(b, "subject", "subject", "");
    textarea(b, "body", "message", "");
    b.push_str("<button type=\"submit\">Send notify</button></form>");

    b.push_str(
        "<form class=\"action-form\" method=\"post\" action=\"/api/ask\"><strong>Ask</strong>",
    );
    input(b, "from", "from", from);
    input(b, "to", "to", to);
    input(b, "subject", "subject", "");
    textarea(b, "body", "question", "");
    b.push_str("<button type=\"submit\">Open ask</button></form>");

    b.push_str("<form class=\"action-form\" method=\"post\" action=\"/api/answer\"><strong>Answer</strong>");
    input(
        b,
        "from",
        "from",
        pending.map(|a| a.askee.as_str()).unwrap_or(from),
    );
    input(
        b,
        "correlation_id",
        "ask id",
        pending.map(|a| a.id.as_str()).unwrap_or(""),
    );
    textarea(b, "body", "answer", "");
    b.push_str("<button type=\"submit\">Answer ask</button></form>");
    b.push_str("</div>");
}

fn render_danger_zone(b: &mut String, snap: &DashboardSnapshot) {
    let cwd = snap
        .peers
        .first()
        .and_then(|p| p.cwd.as_deref())
        .unwrap_or("");
    let kill_target = snap.peers.first().map(|p| p.name.as_str()).unwrap_or("");
    b.push_str("<p class=\"muted\">Destructive session controls are intentionally explicit. Spawn is argv-only JSON and is denied unless the dashboard was started with <code>--write</code>, the command program is trusted by the injector, and <code>spawn_allowed_dirs</code>/<code>WEAVE_SPAWN_DIRS</code> allows the cwd. Kill uses canonical <code>weave_kill_peer</code> and may be coarse for zellij/screen.</p>");
    b.push_str("<div class=\"action-grid\">");
    b.push_str("<form class=\"action-form\" method=\"post\" action=\"/api/spawn-peer\"><strong>Spawn peer</strong>");
    input(b, "name", "new peer", "");
    textarea(
        b,
        "cmd",
        "cmd argv JSON",
        "[\"weave\",\"hook\",\"session\"]",
    );
    input(b, "cwd", "cwd", cwd);
    input(b, "mux", "mux override", "");
    input(b, "circle", "circle", "");
    b.push_str("<label><input name=\"window\" value=\"false\">new window true|false</label>");
    b.push_str("<button type=\"submit\">Spawn via shared handler</button></form>");

    b.push_str("<form class=\"action-form\" method=\"post\" action=\"/api/kill-peer\"><strong>Kill peer</strong>");
    input(b, "name", "peer", kill_target);
    b.push_str("<p class=\"muted\">Preview the peer/mux target first; submitting calls <code>weave_kill_peer</code> through the bearer-gated write API.</p>");
    b.push_str("<button type=\"submit\">Kill selected peer</button></form>");
    b.push_str("</div>");
}

fn render_settings_panel(b: &mut String, snap: &DashboardSnapshot) {
    let s = &snap.settings;
    b.push_str("<div class=\"detail-grid\">");
    detail_item(b, "circle", &s.circle);
    detail_item(
        b,
        "write API",
        if s.write_enabled {
            "enabled"
        } else {
            "read-only"
        },
    );
    detail_item(
        b,
        "spawn allowlist",
        &format!("{} dirs", s.spawn_allowed_dirs.len()),
    );
    detail_item(b, "peer dbs", &s.peer_db_count.to_string());
    detail_item(b, "pull from", &s.pull_from_count.to_string());
    detail_item(
        b,
        "inject pulled",
        if s.inject_pulled {
            "enabled"
        } else {
            "disabled"
        },
    );
    detail_item(
        b,
        "allow inject from",
        &s.allow_inject_from_count
            .map(|n| format!("{n} narrowed"))
            .unwrap_or_else(|| "same as pull_from".to_string()),
    );
    detail_item(b, "bridge identity", &s.bridge_identity);
    detail_item(
        b,
        "telegram",
        if s.telegram_configured {
            "configured"
        } else {
            "not configured"
        },
    );
    detail_item(
        b,
        "slack",
        if s.slack_configured {
            "configured"
        } else {
            "not configured"
        },
    );
    detail_item(
        b,
        "pretooluse approver",
        if s.pretooluse_approver_configured {
            "configured"
        } else {
            "not configured"
        },
    );
    detail_item(
        b,
        "pretooluse timeout",
        &format!("{}s", s.pretooluse_timeout_secs),
    );
    b.push_str("</div>");
    if s.spawn_allowed_dirs.is_empty() {
        b.push_str("<p class=\"muted\">Spawn allowlist is empty, so dashboard/MCP spawn requests are denied by default.</p>");
    } else {
        b.push_str("<h2>Spawn allowed dirs</h2><ul>");
        for dir in s.spawn_allowed_dirs.iter().take(8) {
            b.push_str("<li><code>");
            b.push_str(&html_escape(dir));
            b.push_str("</code></li>");
        }
        b.push_str("</ul>");
    }
    b.push_str("<p class=\"muted\">Token-free JSON: <code>/settings</code> or <code>/api/settings</code>. Secrets are reported only as configured/not configured.</p>");
}

fn input(b: &mut String, name: &str, label: &str, value: &str) {
    b.push_str("<label>");
    b.push_str(&html_escape(label));
    b.push_str("<input name=\"");
    b.push_str(&html_escape(name));
    b.push_str("\" value=\"");
    b.push_str(&html_escape(value));
    b.push_str("\"></label>");
}

fn textarea(b: &mut String, name: &str, label: &str, value: &str) {
    b.push_str("<label>");
    b.push_str(&html_escape(label));
    b.push_str("<textarea name=\"");
    b.push_str(&html_escape(name));
    b.push_str("\">");
    b.push_str(&html_escape(value));
    b.push_str("</textarea></label>");
}

fn render_peers(b: &mut String, snap: &DashboardSnapshot, now: i64) {
    b.push_str("<h2>Sessions / presence</h2>");
    if snap.peers.is_empty() {
        b.push_str("<p class=\"empty\">no sessions</p>");
        return;
    }
    b.push_str(
        "<table><tr><th>name</th><th>mux</th><th>presence</th><th>repo</th><th>last seen</th></tr>",
    );
    for p in &snap.peers {
        let live = now - p.last_seen <= PRESENCE_TTL_SECS;
        let (cls, label) = if live {
            ("live", "live")
        } else {
            ("idle", "idle")
        };
        b.push_str("<tr><td>");
        b.push_str(&html_escape(&p.name));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&p.mux));
        b.push_str("</td><td class=\"");
        b.push_str(cls);
        b.push_str("\">");
        b.push_str(label);
        b.push_str("</td><td>");
        b.push_str(&html_escape(&p.repo));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&fmt_ts(p.last_seen)));
        b.push_str("</td></tr>");
    }
    b.push_str("</table>");
}

fn render_messages(b: &mut String, snap: &DashboardSnapshot) {
    b.push_str("<h2>Recent messages</h2>");
    if snap.messages.is_empty() {
        b.push_str("<p class=\"empty\">no messages</p>");
        return;
    }
    b.push_str("<table><tr><th>ts</th><th>from</th><th>to</th><th>subject</th><th>body</th></tr>");
    for m in &snap.messages {
        b.push_str("<tr><td>");
        b.push_str(&html_escape(&fmt_ts(m.ts)));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&m.sender));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&m.recipient));
        b.push_str("</td><td>");
        b.push_str(&html_escape(m.subject.as_deref().unwrap_or("")));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&m.body));
        b.push_str("</td></tr>");
    }
    b.push_str("</table>");
}

fn render_jobs(b: &mut String, snap: &DashboardSnapshot) {
    b.push_str("<h2>Jobs</h2>");
    if snap.jobs.is_empty() {
        b.push_str("<p class=\"empty\">no jobs</p>");
        return;
    }
    b.push_str("<table><tr><th>id</th><th>title</th><th>state</th><th>owner</th></tr>");
    for j in &snap.jobs {
        b.push_str("<tr><td><code>");
        b.push_str(&html_escape(&j.id));
        b.push_str("</code></td><td>");
        b.push_str(&html_escape(&j.title));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&format!("{:?}", j.state)));
        b.push_str("</td><td>");
        b.push_str(&html_escape(j.owner.as_deref().unwrap_or("")));
        b.push_str("</td></tr>");
    }
    b.push_str("</table>");
}

fn render_leases(b: &mut String, snap: &DashboardSnapshot) {
    b.push_str("<h2>Leases</h2>");
    if snap.leases.is_empty() {
        b.push_str("<p class=\"empty\">no leases</p>");
        return;
    }
    b.push_str("<table><tr><th>resource</th><th>holder</th><th>expires</th></tr>");
    for l in &snap.leases {
        b.push_str("<tr><td>");
        b.push_str(&html_escape(&l.resource));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&l.holder));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&fmt_ts(l.expires)));
        b.push_str("</td></tr>");
    }
    b.push_str("</table>");
}

fn render_schedules(b: &mut String, snap: &DashboardSnapshot) {
    b.push_str("<h2>Schedules</h2>");
    if snap.schedules.is_empty() {
        b.push_str("<p class=\"empty\">no schedules</p>");
        return;
    }
    b.push_str("<table><tr><th>id</th><th>cron</th><th>next run</th><th>to</th><th>body</th></tr>");
    for s in &snap.schedules {
        b.push_str("<tr><td>");
        b.push_str(&html_escape(&s.id.to_string()));
        b.push_str("</td><td><code>");
        b.push_str(&html_escape(&s.cron_expr));
        b.push_str("</code></td><td>");
        b.push_str(&html_escape(&fmt_ts(s.next_run)));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&s.recipient));
        b.push_str("</td><td>");
        b.push_str(&html_escape(&s.body));
        b.push_str("</td></tr>");
    }
    b.push_str("</table>");
}

/// Render the SSE payload pushed on each `GET /events` tick: a compact, already
/// HTML-escaped fragment the page can swap in. Pure; same `(snap, now, host)`
/// determinism as [`render_dashboard`]. Wrapped into a single SSE `data:` frame
/// by [`sse_event`] at the call site.
pub fn render_events_fragment(snap: &DashboardSnapshot, now: i64, host: &str) -> String {
    render_dashboard(snap, now, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weave_core::model::Peer;

    fn peer(name: &str) -> Peer {
        Peer {
            name: name.to_string(),
            mux: "tmux".to_string(),
            target: "%1".to_string(),
            socket: String::new(),
            cwd: None,
            last_seen: 1000,
            pid: None,
            host: String::new(),
            repo: "repo".to_string(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: "default".to_string(),
            role: "peer".to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
            client_session: String::new(),
        }
    }

    fn msg(sender: &str, body: &str) -> Message {
        Message {
            id: 1,
            ts: 1000,
            sender: sender.to_string(),
            recipient: "bob".to_string(),
            subject: None,
            body: body.to_string(),
            in_reply_to: None,
            idempotency_key: None,
            trace_id: None,
            priority: "normal".to_string(),
            superseded_by: None,
            expires_at: None,
            kind: None,
        }
    }

    #[test]
    fn html_escape_round_trips_significant_chars() {
        assert_eq!(html_escape("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(html_escape("x&y"), "x&amp;y");
        assert_eq!(html_escape("\"q\" 'p'"), "&quot;q&quot; &#x27;p&#x27;");
        // plain text untouched
        assert_eq!(html_escape("hello world 123"), "hello world 123");
    }

    #[test]
    fn xss_payload_is_escaped_in_rendered_page() {
        let mut snap = DashboardSnapshot::default();
        snap.peers.push(peer("<script>alert(1)</script>"));
        snap.messages
            .push(msg("evil", "<img src=x onerror=alert('xss')>"));
        let html = render_dashboard(&snap, 1000, "host");
        // The raw payload must NOT appear unescaped anywhere.
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "unescaped peer name leaked: {html}"
        );
        assert!(
            !html.contains("<img src=x onerror=alert('xss')>"),
            "unescaped body leaked: {html}"
        );
        // The escaped forms must be present.
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(&#x27;xss&#x27;)&gt;"));
    }

    #[test]
    fn empty_snapshot_renders_stable_no_data_body() {
        let snap = DashboardSnapshot::default();
        let html = render_dashboard(&snap, 1000, "host");
        assert!(html.contains("no sessions"));
        assert!(html.contains("no messages"));
        assert!(html.contains("no jobs"));
        assert!(html.contains("no leases"));
        assert!(html.contains("no schedules"));
    }

    #[test]
    fn render_is_deterministic_given_inputs() {
        let mut snap = DashboardSnapshot::default();
        snap.peers.push(peer("alice"));
        let a = render_dashboard(&snap, 2000, "h");
        let b = render_dashboard(&snap, 2000, "h");
        assert_eq!(a, b, "render must be pure/deterministic");
    }

    #[test]
    fn presence_label_tracks_ttl() {
        let mut snap = DashboardSnapshot::default();
        snap.peers.push(peer("alice")); // last_seen = 1000
        let live = render_dashboard(&snap, 1000 + PRESENCE_TTL_SECS, "h");
        assert!(live.contains(">live<"), "within TTL should be live");
        let idle = render_dashboard(&snap, 1000 + PRESENCE_TTL_SECS + 1, "h");
        assert!(idle.contains(">idle<"), "past TTL should be idle");
    }

    #[test]
    fn sse_event_frames_per_spec() {
        assert_eq!(sse_event("hello"), "data: hello\n\n");
        // multi-line data splits into one data: field per line, no stray CR
        assert_eq!(sse_event("a\nb"), "data: a\ndata: b\n\n");
        assert_eq!(sse_event("a\r\nb"), "data: a\ndata: b\n\n");
        assert!(!sse_event("x\r\ny").contains('\r'));
    }

    #[test]
    fn route_classification() {
        assert_eq!(route("GET", "/"), Route::Page);
        assert_eq!(route("GET", "/?token=browser"), Route::Page);
        assert_eq!(route("GET", "/events"), Route::Events);
        assert_eq!(route("GET", "/events/stream"), Route::Events);
        assert_eq!(route("GET", "/events?token=browser"), Route::Events);
        assert_eq!(route("GET", "/events?since=msg_1"), Route::EventsJson);
        assert_eq!(route("GET", "/api/events"), Route::EventsJson);
        assert_eq!(route("GET", "/api/snapshot"), Route::SnapshotJson);
        assert_eq!(route("GET", "/peers"), Route::PeersJson);
        assert_eq!(route("GET", "/jobs?view=summary"), Route::JobsJson);
        assert_eq!(route("GET", "/asks/pending"), Route::AsksPendingJson);
        assert_eq!(route("GET", "/health"), Route::HealthJson);
        assert_eq!(route("POST", "/"), Route::JsonRpc);
        assert_eq!(route("GET", "/nope"), Route::NotFound);
        assert_eq!(route("DELETE", "/"), Route::NotFound);
    }
}
