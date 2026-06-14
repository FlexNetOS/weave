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
use weave_core::model::{fmt_ts, Job, Lease, Message, Peer, Schedule};

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
    /// `POST /` — the existing MCP JSON-RPC surface (left untouched).
    JsonRpc,
    /// Anything else — `404`.
    NotFound,
}

/// Classify a request from its method + path. Pure; the caller (http.rs) maps the
/// route to the right writer. `POST /` stays [`Route::JsonRpc`] so the surfaces
/// extension provably does not alter the MCP path.
pub fn route(method: &str, path: &str) -> Route {
    match (method, path) {
        ("GET", "/") => Route::Page,
        ("GET", "/events") => Route::Events,
        ("POST", "/") => Route::JsonRpc,
        _ => Route::NotFound,
    }
}

/// A read-only snapshot of mesh state for one dashboard render. Composed by the
/// caller from existing `Store` reads (`list_peers` / `inbox` / `list_jobs` /
/// `list_leases` / `list_schedules`) — this module never opens the DB.
#[derive(Debug, Default, Clone)]
pub struct DashboardSnapshot {
    pub peers: Vec<Peer>,
    pub messages: Vec<Message>,
    pub jobs: Vec<Job>,
    pub leases: Vec<Lease>,
    pub schedules: Vec<Schedule>,
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
        "body{font-family:system-ui,sans-serif;margin:1.5rem;background:#0d1117;color:#c9d1d9}\
         h1{font-size:1.3rem}h2{font-size:1rem;margin-top:1.5rem;border-bottom:1px solid #30363d;padding-bottom:.25rem}\
         table{border-collapse:collapse;width:100%;font-size:.85rem}\
         td,th{text-align:left;padding:.2rem .5rem;border-bottom:1px solid #21262d;vertical-align:top}\
         .live{color:#3fb950}.idle{color:#8b949e}.empty{color:#8b949e;font-style:italic}\
         code{color:#79c0ff}",
    );
    b.push_str("</style></head><body>");
    b.push_str("<h1>weave dashboard <code>");
    b.push_str(&html_escape(host));
    b.push_str("</code></h1>");
    b.push_str("<p class=\"idle\">read-only · ");
    b.push_str(&html_escape(&fmt_ts(now)));
    b.push_str("</p>");

    render_peers(&mut b, snap, now);
    render_messages(&mut b, snap);
    render_jobs(&mut b, snap);
    render_leases(&mut b, snap);
    render_schedules(&mut b, snap);

    b.push_str("</body></html>");
    b
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
        assert_eq!(route("GET", "/events"), Route::Events);
        assert_eq!(route("POST", "/"), Route::JsonRpc);
        assert_eq!(route("GET", "/nope"), Route::NotFound);
        assert_eq!(route("DELETE", "/"), Route::NotFound);
    }
}
