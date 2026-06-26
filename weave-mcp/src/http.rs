//! Minimal localhost-only HTTP JSON-RPC transport for the weave MCP server.
//!
//! Uses `std::net::TcpListener` — no async runtime, no extra HTTP dependencies.
//! Accepts POST requests with `Content-Length`, verifies `Authorization: Bearer`,
//! dispatches through [`dispatch_request`], and returns JSON-RPC responses.

use crate::mcp::{dispatch_request, PullConsent};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use weave_core::config::StoreSource;
use weave_inject::Injector;

const DEFAULT_PROTOCOL: &str = "HTTP/1.1";

/// WL-056 / ADR-0005: is `bind` a loopback address (the safe default that needs no
/// token)? Parses the address as an `IpAddr` and asks the stdlib; a bare `localhost`
/// is treated as loopback too. A non-parseable / non-loopback address is NOT
/// loopback, so `serve_http` will require a bearer token for it (fail-closed). This
/// is a pure function (unit-tested) — the routable-bind fail-closed gate rests on it.
fn is_loopback_bind(bind: &str) -> bool {
    let host = bind.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // Unknown / unparseable host is treated as NOT loopback → token required
        // (fail-closed: never assume an unrecognized bind is safe).
        Err(_) => false,
    }
}

/// WL-048: how long the SSE accept thread sleeps between dashboard snapshots
/// pushed to a `GET /events` client. A bounded interval (no busy loop); a
/// keep-alive comment is interleaved so intermediaries do not time out.
#[cfg(feature = "surfaces")]
const SSE_TICK_SECS: u64 = 2;

/// Start a blocking HTTP server on `<bind>:port`. Only POST / is accepted.
/// Bearer token is required unless `token` is empty. Dangerous tools are
/// filtered unless `dangerous` is true.
///
/// `bind` defaults to `127.0.0.1` (loopback — the only safe default, posture
/// unchanged from before WL-056). Cross-machine PUSH (ADR-0005) requires the
/// operator to *deliberately* expose B by passing a routable `--bind` (e.g.
/// `0.0.0.0` or a Tailscale address). FAIL-CLOSED: a non-loopback bind with an
/// EMPTY bearer token is refused — weave never opens an unauthenticated listener on
/// a routable address.
#[allow(clippy::too_many_arguments)]
pub fn serve_http(
    store: &dyn weave_core::store::Store,
    me_default: Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: Vec<StoreSource>,
    pull: PullConsent,
    injector: &dyn Injector,
    bind: &str,
    port: u16,
    token: &str,
    dangerous: bool,
) -> anyhow::Result<()> {
    // FAIL-CLOSED: refuse to bind a routable address without a bearer token (no open
    // listener on the network). Checked BEFORE TcpListener::bind so we never even
    // open the socket. A loopback bind keeps today's posture (empty token allowed).
    if !is_loopback_bind(bind) && token.is_empty() {
        anyhow::bail!(
            "refusing to bind a routable address without a bearer token \
             (bind='{bind}'); pass --token or bind 127.0.0.1"
        );
    }
    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr)?;
    log(&format!("HTTP MCP server listening on http://{addr}"));
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                if let Err(e) = handle_connection(
                    &mut s,
                    store,
                    &me_default,
                    nudge_template,
                    &extra_dbs,
                    &pull,
                    injector,
                    token,
                    dangerous,
                ) {
                    log(&format!("connection error: {e}"));
                }
            }
            Err(e) => log(&format!("accept error: {e}")),
        }
    }
    Ok(())
}

/// WL-048 / ADR-0004: start the **read-only human dashboard** HTTP server on
/// `127.0.0.1:port`. Distinct from [`serve_http`] (the MCP JSON-RPC surface): this
/// serves ONLY the GET `/` (HTML) and `/events` (SSE) routes, never mutates, and
/// spawns a short-lived `std::thread` per accepted connection so a long-lived SSE
/// stream cannot starve other requests (still NO async runtime). Each connection
/// thread opens its OWN read-only `Store` handle via `store_factory` — `Store` is
/// `Send` but not `Sync`, so a shared `&dyn Store` cannot cross the thread
/// boundary; a per-connection handle is the clean, lock-free answer for read-only
/// snapshots. Bearer auth (WL-022) gates both routes; an empty `token` ⇒ open.
#[cfg(feature = "surfaces")]
#[allow(clippy::too_many_arguments)]
pub fn serve_dashboard<F>(
    bind: &str,
    port: u16,
    token: &str,
    write: bool,
    me_default: Option<String>,
    injector: &(dyn Injector + Sync),
    store_factory: F,
) -> anyhow::Result<()>
where
    F: Fn() -> anyhow::Result<Box<dyn weave_core::store::Store>> + Send + Sync + 'static,
{
    // FAIL-CLOSED: same routable-bind-requires-token rule as `serve_http`. The
    // `POST /api` write surface (`--write`) is the cross-machine push receive seam,
    // so an exposed dashboard must carry a token too.
    if !is_loopback_bind(bind) && token.is_empty() {
        anyhow::bail!(
            "refusing to bind a routable address without a bearer token \
             (bind='{bind}'); pass --token or bind 127.0.0.1"
        );
    }
    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr)?;
    log(&format!(
        "dashboard listening on http://{addr} ({})",
        if write { "read-write" } else { "read-only" }
    ));
    let token = token.to_string();
    let factory = std::sync::Arc::new(store_factory);
    std::thread::scope(|scope| {
        for stream in listener.incoming() {
            match stream {
                Ok(mut s) => {
                    let token = token.clone();
                    let factory = std::sync::Arc::clone(&factory);
                    let me_default = me_default.clone();
                    scope.spawn(move || {
                        let store = match factory() {
                            Ok(st) => st,
                            Err(e) => {
                                log(&format!("dashboard store open error: {e}"));
                                return;
                            }
                        };
                        if let Err(e) = handle_dashboard_connection(
                            &mut s,
                            store.as_ref(),
                            &token,
                            write,
                            &me_default,
                            injector,
                        ) {
                            log(&format!("dashboard connection error: {e}"));
                        }
                    });
                }
                Err(e) => log(&format!("dashboard accept error: {e}")),
            }
        }
    });
    Ok(())
}

/// Handle one dashboard connection. GET serves the read-only HTML/SSE routes. When
/// the server was started with `write` (WL-052a: `weave dashboard --write`), a
/// bearer-gated `POST /api` accepts a JSON-RPC body and routes it through the **same**
/// [`dispatch_request`] the MCP/CLI surfaces use — no parallel write path, so every
/// invariant (caps, parameterized SQL, destructive gating, nudge-inject) is inherited.
/// Without `write`, any POST is refused 403 (the default read-only posture).
#[cfg(feature = "surfaces")]
#[allow(clippy::too_many_arguments)]
fn handle_dashboard_connection(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    token: &str,
    write: bool,
    me_default: &Option<String>,
    injector: &dyn Injector,
) -> anyhow::Result<()> {
    use crate::dashboard::{route, Route};
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    let mut rl = first_line.split_whitespace();
    let method = rl.next().unwrap_or("").to_string();
    let path = rl.next().unwrap_or("").to_string();

    if method == "GET" {
        let auth_ok = read_headers_auth_only(&mut reader, token, Some(&path))?;
        if !auth_ok {
            write_http(stream, 401, b"Unauthorized")?;
            return Ok(());
        }
        return match route(&method, &path) {
            Route::Page => serve_dashboard_page(stream, store),
            Route::Events => serve_dashboard_events(stream, store),
            _ => {
                write_http(stream, 404, b"Not Found")?;
                Ok(())
            }
        };
    }

    if method == "POST" {
        if !write {
            write_http(
                stream,
                403,
                b"Dashboard is read-only. Start `weave dashboard --write` to enable the action API.",
            )?;
            return Ok(());
        }
        if path != "/api" {
            write_http(stream, 404, b"Not Found")?;
            return Ok(());
        }
        // Parse headers (content-length + bearer), mirroring the JSON-RPC POST path.
        let mut content_length = 0usize;
        let mut auth_ok = token.is_empty();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                content_length = lower
                    .split(':')
                    .nth(1)
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
            }
            if lower.starts_with("authorization:") {
                let provided = line.split(':').nth(1).unwrap_or("").trim();
                if provided == format!("Bearer {token}") || provided == format!("bearer {token}") {
                    auth_ok = true;
                }
            }
        }
        if !auth_ok {
            write_http(stream, 401, b"Unauthorized")?;
            return Ok(());
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let req: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                write_http(stream, 400, format!("Invalid JSON: {e}").as_bytes())?;
                return Ok(());
            }
        };
        // The SAME handler as MCP/CLI. `dangerous = true` because `--write` is the
        // operator's explicit opt-in to mutations on this bearer-gated local surface.
        let resp = dispatch_request(
            store,
            me_default,
            None,
            &[],
            &PullConsent::empty(),
            &req,
            injector,
            true,
        );
        let resp_body = resp.unwrap_or_else(|| "{}".to_string());
        write_http(stream, 200, resp_body.as_bytes())?;
        return Ok(());
    }

    write_http(stream, 405, b"Method Not Allowed")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_connection(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    injector: &dyn Injector,
    token: &str,
    dangerous: bool,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;

    // Parse the request line into (method, path). The POST/JSON-RPC path below is
    // unchanged; the `surfaces` build additionally accepts read-only GET routes.
    let mut rl = first_line.split_whitespace();
    let method = rl.next().unwrap_or("").to_string();
    #[cfg_attr(not(feature = "surfaces"), allow(unused_variables))]
    let path = rl.next().unwrap_or("").to_string();

    // Without the surfaces feature, only POST / is accepted (byte-identical to the
    // original behavior).
    #[cfg(not(feature = "surfaces"))]
    if method != "POST" {
        write_http(stream, 405, b"Method Not Allowed")?;
        return Ok(());
    }

    // With surfaces, a GET to an unknown path is a 404 (the dashboard adds `/` and
    // `/events`); any other non-POST method is still 405.
    #[cfg(feature = "surfaces")]
    {
        use crate::dashboard::{route, Route};
        if method == "GET" {
            // GET routes still require bearer auth; parse headers to find it.
            let auth_ok = read_headers_auth_only(&mut reader, token, Some(&path))?;
            if !auth_ok {
                write_http(stream, 401, b"Unauthorized")?;
                return Ok(());
            }
            match route(&method, &path) {
                Route::Page => return serve_dashboard_page(stream, store),
                Route::Events => return serve_dashboard_events(stream, store),
                _ => {
                    write_http(stream, 404, b"Not Found")?;
                    return Ok(());
                }
            }
        } else if method != "POST" {
            write_http(stream, 405, b"Method Not Allowed")?;
            return Ok(());
        }
    }

    // Parse headers.
    let mut content_length = 0usize;
    let mut auth_ok = token.is_empty();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            content_length = lower
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            let provided = line.split(':').nth(1).unwrap_or("").trim();
            if provided == format!("Bearer {token}") || provided == format!("bearer {token}") {
                auth_ok = true;
            }
        }
    }

    if !auth_ok {
        write_http(stream, 401, b"Unauthorized")?;
        return Ok(());
    }

    // Read body.
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    // Dispatch JSON-RPC.
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            write_http(stream, 400, format!("Invalid JSON: {e}").as_bytes())?;
            return Ok(());
        }
    };

    let resp = dispatch_request(
        store,
        me_default,
        nudge_template,
        extra_dbs,
        pull,
        &req,
        injector,
        dangerous,
    );

    let resp_body = resp.unwrap_or_else(|| "{}".to_string());
    write_http(stream, 200, resp_body.as_bytes())?;
    Ok(())
}

fn write_http(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Unknown",
    };
    let header = format!(
        "{DEFAULT_PROTOCOL} {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn log(msg: &str) {
    eprintln!("[weave-http] {msg}");
}

// ---------------------------------------------------------------------------
// WL-048 / ADR-0004: read-only human dashboard (surfaces feature only).
// All of the following are GET-only, never mutate the Store, reuse the WL-022
// bearer-auth, and HTML-escape every Store-derived string via `dashboard`.
// ---------------------------------------------------------------------------

/// Drain the request headers, returning whether bearer auth passed (used for the
/// GET dashboard routes, which carry no body). Mirrors the auth logic in the POST
/// path; an empty configured token means auth is open.
#[cfg(feature = "surfaces")]
fn read_headers_auth_only<R: BufRead>(
    reader: &mut R,
    token: &str,
    path: Option<&str>,
) -> anyhow::Result<bool> {
    let mut auth_ok = token.is_empty() || query_token_matches(path, token);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.is_empty() || line == "\r\n" || line == "\n" {
            break;
        }
        if line.to_lowercase().starts_with("authorization:") {
            let provided = line.split(':').nth(1).unwrap_or("").trim();
            if provided == format!("Bearer {token}") || provided == format!("bearer {token}") {
                auth_ok = true;
            }
        }
        if line.to_lowercase().starts_with("cookie:") && cookie_token_matches(&line, token) {
            auth_ok = true;
        }
    }
    Ok(auth_ok)
}

#[cfg(feature = "surfaces")]
fn query_token_matches(path: Option<&str>, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let Some(path) = path else {
        return false;
    };
    let Some((_, query)) = path.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        matches!(k, "token" | "access_token") && v == token
    })
}

#[cfg(feature = "surfaces")]
fn cookie_token_matches(line: &str, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let cookie = line.split_once(':').map(|(_, v)| v).unwrap_or("");
    cookie.split(';').any(|part| {
        let (k, v) = part.trim().split_once('=').unwrap_or(("", ""));
        k == "weave_dashboard_token" && v == token
    })
}

/// Compose a read-only [`dashboard::DashboardSnapshot`] from existing `Store`
/// reads. No new SQL, no new trait method — just the existing list/inbox calls.
#[cfg(feature = "surfaces")]
fn build_snapshot(
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<crate::dashboard::DashboardSnapshot> {
    use weave_core::model::JobFilter;
    let peers = store.list_peers().unwrap_or_default();
    // Recent messages across the mesh, composed from existing Store reads (NO new
    // SQL / trait method, per ADR-0004): union each known peer's recent history,
    // dedup by message id, sort newest-first, cap. Bounded by peer count × the
    // per-peer limit; both are small.
    let mut by_id: std::collections::HashMap<i64, weave_core::model::Message> =
        std::collections::HashMap::new();
    for p in &peers {
        for m in store.history(&p.name, None, 50).unwrap_or_default() {
            by_id.entry(m.id).or_insert(m);
        }
    }
    let mut messages: Vec<weave_core::model::Message> = by_id.into_values().collect();
    messages.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.id.cmp(&a.id)));
    messages.truncate(50);
    let jobs = store
        .list_jobs(JobFilter::default(), 50)
        .unwrap_or_default();
    let leases = store.list_leases(50).unwrap_or_default();
    let schedules = store.list_schedules("", 50).unwrap_or_default();
    Ok(crate::dashboard::DashboardSnapshot {
        peers,
        messages,
        jobs,
        leases,
        schedules,
    })
}

/// `GET /` → render the dashboard HTML page once and close.
#[cfg(feature = "surfaces")]
fn serve_dashboard_page(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store)?;
    let host = weave_core::config::this_host();
    let html = crate::dashboard::render_dashboard(&snap, weave_core::model::now(), &host);
    write_http_html(stream, &html)?;
    Ok(())
}

/// `GET /events` → open a long-lived SSE stream pushing a fresh dashboard
/// fragment on a bounded interval, interleaved with keep-alive comments. A write
/// error (client disconnect) closes the connection cleanly. This runs on the
/// per-connection thread spawned by [`serve_http`] under the surfaces feature, so
/// it cannot starve the MCP port.
#[cfg(feature = "surfaces")]
fn serve_dashboard_events(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<()> {
    let header = format!(
        "{DEFAULT_PROTOCOL} 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return Ok(());
    }
    let host = weave_core::config::this_host();
    loop {
        let snap = build_snapshot(store)?;
        let fragment =
            crate::dashboard::render_events_fragment(&snap, weave_core::model::now(), &host);
        let frame = crate::dashboard::sse_event(&fragment);
        if stream.write_all(frame.as_bytes()).is_err() || stream.flush().is_err() {
            return Ok(()); // client disconnected — close cleanly
        }
        std::thread::sleep(std::time::Duration::from_secs(SSE_TICK_SECS));
        if stream
            .write_all(crate::dashboard::sse_keepalive().as_bytes())
            .is_err()
        {
            return Ok(());
        }
    }
}

/// Write a `200 OK text/html` response for the dashboard page.
#[cfg(feature = "surfaces")]
fn write_http_html(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let header = format!(
        "{DEFAULT_PROTOCOL} 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::is_loopback_bind;

    #[test]
    fn loopback_addresses_are_recognized() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("127.0.0.5"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("  127.0.0.1  "));
    }

    #[test]
    fn routable_or_unknown_addresses_are_not_loopback() {
        // Routable → requires a token (fail-closed).
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.10"));
        assert!(!is_loopback_bind("10.0.0.1"));
        assert!(!is_loopback_bind("100.64.0.1")); // Tailscale CGNAT range
                                                  // Unparseable / hostname → NOT loopback (never assume an unknown bind is safe).
        assert!(!is_loopback_bind("example.com"));
        assert!(!is_loopback_bind(""));
    }
}
