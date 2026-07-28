//! Minimal localhost-only HTTP JSON-RPC transport for the weave MCP server.
//!
//! Uses `std::net::TcpListener` — no async runtime, no extra HTTP dependencies.
//! Accepts POST requests with `Content-Length`, verifies `Authorization: Bearer`,
//! dispatches through [`dispatch_request`], and returns JSON-RPC responses.

use crate::mcp::{dispatch_push_request, dispatch_request, PullConsent};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use weave_core::config::StoreSource;
use weave_inject::Injector;

const DEFAULT_PROTOCOL: &str = "HTTP/1.1";
const HTTP_IO_TIMEOUT_SECS: u64 = 10;
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADER_COUNT: usize = 100;
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const MAX_HTTP_CONNECTIONS: usize = 64;

struct ConnectionSlot(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

fn claim_connection_slot(
    active: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Option<ConnectionSlot> {
    active
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |count| (count < MAX_HTTP_CONNECTIONS).then_some(count + 1),
        )
        .ok()
        .map(|_| ConnectionSlot(std::sync::Arc::clone(active)))
}

fn configure_http_stream(stream: &TcpStream) -> std::io::Result<()> {
    let timeout = Some(std::time::Duration::from_secs(HTTP_IO_TIMEOUT_SECS));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    stream.set_nodelay(true)?;
    Ok(())
}

/// Read one UTF-8 HTTP line without allowing `BufRead::read_line` to grow a
/// caller-controlled String without bound.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    max_bytes: usize,
) -> anyhow::Result<usize> {
    let started = std::time::Instant::now();
    let mut bytes = Vec::with_capacity(max_bytes.min(1024));
    loop {
        if started.elapsed().as_secs() >= HTTP_IO_TIMEOUT_SECS {
            anyhow::bail!("HTTP line read exceeded the time limit");
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(take) > max_bytes {
            anyhow::bail!("HTTP line exceeds the {max_bytes}-byte limit");
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    let len = bytes.len();
    *out = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("HTTP line is not UTF-8"))?;
    Ok(len)
}

fn read_bounded_header_line<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    total_bytes: &mut usize,
    count: &mut usize,
    started: &std::time::Instant,
) -> anyhow::Result<usize> {
    if started.elapsed().as_secs() >= HTTP_IO_TIMEOUT_SECS {
        anyhow::bail!("HTTP header read exceeded the time limit");
    }
    if *count >= MAX_HEADER_COUNT {
        anyhow::bail!("HTTP request exceeds the {MAX_HEADER_COUNT}-header limit");
    }
    let n = read_bounded_line(reader, out, MAX_HEADER_LINE_BYTES)?;
    *count += 1;
    *total_bytes = (*total_bytes).saturating_add(n);
    if *total_bytes > MAX_HEADER_BYTES {
        anyhow::bail!("HTTP headers exceed the {MAX_HEADER_BYTES}-byte limit");
    }
    Ok(n)
}

fn read_bounded_body<R: Read>(reader: &mut R, content_length: usize) -> anyhow::Result<Vec<u8>> {
    if content_length > MAX_HTTP_BODY_BYTES {
        anyhow::bail!("HTTP body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit");
    }
    let started = std::time::Instant::now();
    let mut body = vec![0u8; content_length];
    let mut offset = 0usize;
    while offset < body.len() {
        if started.elapsed().as_secs() >= HTTP_IO_TIMEOUT_SECS {
            anyhow::bail!("HTTP body read exceeded the time limit");
        }
        let read = reader.read(&mut body[offset..])?;
        if read == 0 {
            anyhow::bail!("HTTP body ended before Content-Length bytes were received");
        }
        offset += read;
    }
    Ok(body)
}

/// WL-056 / ADR-0005: is `bind` a loopback address (the safe default that needs no
/// token)? Parses the address as an `IpAddr` and asks the stdlib; a bare `localhost`
/// is treated as loopback too. A non-parseable / non-loopback address is NOT
/// loopback, so `serve_http` will require a bearer token for it (fail-closed). This
/// is a pure function (unit-tested) — the routable-bind fail-closed gate rests on it.
pub fn is_loopback_bind(bind: &str) -> bool {
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

fn validate_separate_push_token(token: &str, push_token: Option<&str>) -> anyhow::Result<()> {
    if let Some(push_token) = push_token {
        if push_token.is_empty() {
            anyhow::bail!("push token must not be empty");
        }
        if push_token == token {
            anyhow::bail!(
                "push token must differ from the operator token so push authority stays isolated"
            );
        }
    }
    Ok(())
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
pub fn serve_http<F>(
    me_default: Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: Vec<StoreSource>,
    pull: PullConsent,
    injector: &(dyn Injector + Sync),
    bind: &str,
    port: u16,
    token: &str,
    push_token: Option<&str>,
    dangerous: bool,
    store_factory: F,
) -> anyhow::Result<()>
where
    F: Fn() -> anyhow::Result<Box<dyn weave_core::store::Store>> + Send + Sync + 'static,
{
    validate_separate_push_token(token, push_token)?;
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
    let token = token.to_string();
    let push_token = push_token.map(str::to_string);
    let nudge_template = nudge_template.map(str::to_string);
    let extra_dbs = std::sync::Arc::new(extra_dbs);
    let pull = std::sync::Arc::new(pull);
    let factory = std::sync::Arc::new(store_factory);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for stream in listener.incoming() {
            match stream {
                Ok(mut s) => {
                    if let Err(e) = configure_http_stream(&s) {
                        log(&format!("connection timeout setup error: {e}"));
                        continue;
                    }
                    let Some(slot) = claim_connection_slot(&active) else {
                        let _ = write_http(&mut s, 503, b"Server busy");
                        continue;
                    };
                    let token = token.clone();
                    let push_token = push_token.clone();
                    let nudge_template = nudge_template.clone();
                    let me_default = me_default.clone();
                    let extra_dbs = std::sync::Arc::clone(&extra_dbs);
                    let pull = std::sync::Arc::clone(&pull);
                    let factory = std::sync::Arc::clone(&factory);
                    scope.spawn(move || {
                        let _slot = slot;
                        if let Err(e) = handle_connection(
                            &mut s,
                            factory.as_ref(),
                            &me_default,
                            nudge_template.as_deref(),
                            extra_dbs.as_slice(),
                            pull.as_ref(),
                            injector,
                            &token,
                            push_token.as_deref(),
                            dangerous,
                        ) {
                            log(&format!("connection error: {e}"));
                        }
                    });
                }
                Err(e) => log(&format!("accept error: {e}")),
            }
        }
    });
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
    push_token: Option<&str>,
    write: bool,
    me_default: Option<String>,
    injector: &(dyn Injector + Sync),
    store_factory: F,
) -> anyhow::Result<()>
where
    F: Fn() -> anyhow::Result<Box<dyn weave_core::store::Store>> + Send + Sync + 'static,
{
    validate_separate_push_token(token, push_token)?;
    // FAIL-CLOSED: same routable-bind-requires-token rule as `serve_http`. The
    // Dashboard/operator access still requires its own token on a routable bind.
    // Cross-machine push uses the distinct, push-only `/push` credential.
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
    let push_token = push_token.map(str::to_string);
    let factory = std::sync::Arc::new(store_factory);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for stream in listener.incoming() {
            match stream {
                Ok(mut s) => {
                    if let Err(e) = configure_http_stream(&s) {
                        log(&format!("dashboard timeout setup error: {e}"));
                        continue;
                    }
                    let Some(slot) = claim_connection_slot(&active) else {
                        let _ = write_http(&mut s, 503, b"Server busy");
                        continue;
                    };
                    let token = token.clone();
                    let push_token = push_token.clone();
                    let factory = std::sync::Arc::clone(&factory);
                    let me_default = me_default.clone();
                    scope.spawn(move || {
                        let _slot = slot;
                        if let Err(e) = handle_dashboard_connection(
                            &mut s,
                            factory.as_ref(),
                            &token,
                            push_token.as_deref(),
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
    store_factory: &(dyn Fn() -> anyhow::Result<Box<dyn weave_core::store::Store>> + Send + Sync),
    token: &str,
    push_token: Option<&str>,
    write: bool,
    me_default: &Option<String>,
    injector: &dyn Injector,
) -> anyhow::Result<()> {
    use crate::dashboard::{route, Route};
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    read_bounded_line(&mut reader, &mut first_line, MAX_REQUEST_LINE_BYTES)?;
    let mut rl = first_line.split_whitespace();
    let method = rl.next().unwrap_or("").to_string();
    let path = rl.next().unwrap_or("").to_string();

    if method == "GET" {
        let auth_ok = read_headers_auth_only(&mut reader, token, Some(&path))?;
        if !auth_ok {
            write_http(stream, 401, b"Unauthorized")?;
            return Ok(());
        }
        let store = store_factory().map_err(|e| anyhow::anyhow!("opening dashboard store: {e}"))?;
        if let Some(peer) = transcript_peer_from_path(&path) {
            return serve_dashboard_peer_transcript_json(stream, store.as_ref(), peer, &path);
        }
        if let Some(job_id) = job_status_id_from_path(&path) {
            return serve_dashboard_job_status_json(stream, store.as_ref(), job_id);
        }
        if let Some(job_id) = job_result_id_from_path(&path) {
            return serve_dashboard_job_result_json(stream, store.as_ref(), job_id);
        }
        return match route(&method, &path) {
            Route::Page => serve_dashboard_page(stream, store.as_ref(), write),
            Route::Events => serve_dashboard_events(stream, store.as_ref(), write),
            Route::SnapshotJson => serve_dashboard_snapshot_json(stream, store.as_ref(), write),
            Route::PeersJson => serve_dashboard_peers_json(stream, store.as_ref()),
            Route::EventsJson => serve_dashboard_events_json(stream, store.as_ref(), &path),
            Route::JobsJson => serve_dashboard_jobs_json(stream, store.as_ref()),
            Route::AsksPendingJson => serve_dashboard_asks_pending_json(stream, store.as_ref()),
            Route::SettingsJson => serve_dashboard_settings_json(stream, store.as_ref(), write),
            Route::HealthJson => serve_dashboard_health_json(stream),
            _ => {
                write_http(stream, 404, b"Not Found")?;
                Ok(())
            }
        };
    }

    if method == "POST" {
        let push_only = path_without_query(&path) == "/push";
        let Some(required_token) = (if push_only { push_token } else { Some(token) }) else {
            write_http(stream, 404, b"Not Found")?;
            return Ok(());
        };
        if !push_only && !write {
            write_http(
                stream,
                403,
                b"Dashboard is read-only. Start `weave dashboard --write` to enable the action API.",
            )?;
            return Ok(());
        }
        let action = (!push_only).then(|| dashboard_action_tool(&path)).flatten();
        if !push_only && path_without_query(&path) != "/api" && action.is_none() {
            write_http(stream, 404, b"Not Found")?;
            return Ok(());
        }
        // Parse headers (content-length + bearer/cookie), mirroring the JSON-RPC POST
        // path while also allowing browser forms authenticated by the dashboard cookie.
        let mut content_length = 0usize;
        let mut auth_ok = !push_only
            && (required_token.is_empty() || query_token_matches(Some(&path), required_token));
        let mut header_bytes = 0usize;
        let mut header_count = 0usize;
        let header_started = std::time::Instant::now();
        loop {
            let mut line = String::new();
            let n = read_bounded_header_line(
                &mut reader,
                &mut line,
                &mut header_bytes,
                &mut header_count,
                &header_started,
            )?;
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                content_length = lower
                    .split(':')
                    .nth(1)
                    .ok_or_else(|| anyhow::anyhow!("invalid Content-Length header"))?
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid Content-Length header"))?;
                if content_length > MAX_HTTP_BODY_BYTES {
                    anyhow::bail!("HTTP body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit");
                }
            }
            if lower.starts_with("transfer-encoding:") {
                anyhow::bail!("Transfer-Encoding is not supported");
            }
            if lower.starts_with("authorization:") {
                let provided = line.split(':').nth(1).unwrap_or("").trim();
                if provided == format!("Bearer {required_token}")
                    || provided == format!("bearer {required_token}")
                {
                    auth_ok = true;
                }
            }
            if !push_only
                && lower.starts_with("cookie:")
                && cookie_token_matches(&line, required_token)
            {
                auth_ok = true;
            }
        }
        if !auth_ok {
            write_http(stream, 401, b"Unauthorized")?;
            return Ok(());
        }
        let body = read_bounded_body(&mut reader, content_length)?;
        let req: Value = if let Some(tool) = action {
            dashboard_action_request(tool, &body)?
        } else {
            match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    write_http(stream, 400, format!("Invalid JSON: {e}").as_bytes())?;
                    return Ok(());
                }
            }
        };
        let store = store_factory().map_err(|e| anyhow::anyhow!("opening dashboard store: {e}"))?;
        let resp = if push_only {
            dispatch_push_request(
                store.as_ref(),
                me_default,
                &PullConsent::empty(),
                &req,
                injector,
            )
        } else {
            // The operator token and `--write` are both required for the full
            // dashboard action surface. The separately scoped push credential can
            // never reach this branch.
            dispatch_request(
                store.as_ref(),
                me_default,
                None,
                &[],
                &PullConsent::empty(),
                &req,
                injector,
                true,
            )
        };
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
    store_factory: &(dyn Fn() -> anyhow::Result<Box<dyn weave_core::store::Store>> + Send + Sync),
    me_default: &Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: &[StoreSource],
    pull: &PullConsent,
    injector: &dyn Injector,
    token: &str,
    push_token: Option<&str>,
    dangerous: bool,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    read_bounded_line(&mut reader, &mut first_line, MAX_REQUEST_LINE_BYTES)?;

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
            let store = store_factory()
                .map_err(|e| anyhow::anyhow!("opening HTTP store after authentication: {e}"))?;
            if let Some(peer) = transcript_peer_from_path(&path) {
                return serve_dashboard_peer_transcript_json(stream, store.as_ref(), peer, &path);
            }
            if let Some(job_id) = job_status_id_from_path(&path) {
                return serve_dashboard_job_status_json(stream, store.as_ref(), job_id);
            }
            if let Some(job_id) = job_result_id_from_path(&path) {
                return serve_dashboard_job_result_json(stream, store.as_ref(), job_id);
            }
            match route(&method, &path) {
                Route::Page => return serve_dashboard_page(stream, store.as_ref(), dangerous),
                Route::Events => return serve_dashboard_events(stream, store.as_ref(), dangerous),
                Route::SnapshotJson => {
                    return serve_dashboard_snapshot_json(stream, store.as_ref(), dangerous)
                }
                Route::PeersJson => return serve_dashboard_peers_json(stream, store.as_ref()),
                Route::EventsJson => {
                    return serve_dashboard_events_json(stream, store.as_ref(), &path)
                }
                Route::JobsJson => return serve_dashboard_jobs_json(stream, store.as_ref()),
                Route::AsksPendingJson => {
                    return serve_dashboard_asks_pending_json(stream, store.as_ref())
                }
                Route::SettingsJson => {
                    return serve_dashboard_settings_json(stream, store.as_ref(), dangerous)
                }
                Route::HealthJson => return serve_dashboard_health_json(stream),
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

    let push_only = path_without_query(&path) == "/push";
    if !push_only && path_without_query(&path) != "/" {
        write_http(stream, 404, b"Not Found")?;
        return Ok(());
    }
    let Some(required_token) = (if push_only { push_token } else { Some(token) }) else {
        write_http(stream, 404, b"Not Found")?;
        return Ok(());
    };

    // Parse headers.
    let mut content_length = 0usize;
    let mut auth_ok = required_token.is_empty();
    let mut header_bytes = 0usize;
    let mut header_count = 0usize;
    let header_started = std::time::Instant::now();
    loop {
        let mut line = String::new();
        let n = read_bounded_header_line(
            &mut reader,
            &mut line,
            &mut header_bytes,
            &mut header_count,
            &header_started,
        )?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            content_length = lower
                .split(':')
                .nth(1)
                .ok_or_else(|| anyhow::anyhow!("invalid Content-Length header"))?
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid Content-Length header"))?;
            if content_length > MAX_HTTP_BODY_BYTES {
                anyhow::bail!("HTTP body exceeds the {MAX_HTTP_BODY_BYTES}-byte limit");
            }
        }
        if lower.starts_with("transfer-encoding:") {
            anyhow::bail!("Transfer-Encoding is not supported");
        }
        if lower.starts_with("authorization:") {
            let provided = line.split(':').nth(1).unwrap_or("").trim();
            if provided == format!("Bearer {required_token}")
                || provided == format!("bearer {required_token}")
            {
                auth_ok = true;
            }
        }
    }

    if !auth_ok {
        write_http(stream, 401, b"Unauthorized")?;
        return Ok(());
    }

    // Read body.
    let body = read_bounded_body(&mut reader, content_length)?;

    // Dispatch JSON-RPC.
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            write_http(stream, 400, format!("Invalid JSON: {e}").as_bytes())?;
            return Ok(());
        }
    };

    let store = store_factory()
        .map_err(|e| anyhow::anyhow!("opening HTTP store after authentication: {e}"))?;
    let resp = if push_only {
        dispatch_push_request(store.as_ref(), me_default, pull, &req, injector)
    } else {
        dispatch_request(
            store.as_ref(),
            me_default,
            nudge_template,
            extra_dbs,
            pull,
            &req,
            injector,
            dangerous,
        )
    };

    let resp_body = resp.unwrap_or_else(|| "{}".to_string());
    write_http(stream, 200, resp_body.as_bytes())?;
    Ok(())
}

fn write_http(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
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

#[cfg(feature = "surfaces")]
fn write_http_json(stream: &mut TcpStream, body: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(body).unwrap_or_else(|_| "{}".to_string());
    let header = format!(
        "{DEFAULT_PROTOCOL} 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
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
    let mut header_bytes = 0usize;
    let mut header_count = 0usize;
    let header_started = std::time::Instant::now();
    loop {
        let mut line = String::new();
        let n = read_bounded_header_line(
            reader,
            &mut line,
            &mut header_bytes,
            &mut header_count,
            &header_started,
        )?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if line.to_lowercase().starts_with("transfer-encoding:") {
            anyhow::bail!("Transfer-Encoding is not supported");
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

fn path_without_query(path: &str) -> &str {
    path.split_once('?').map(|(p, _)| p).unwrap_or(path)
}

#[cfg(feature = "surfaces")]
fn dashboard_action_tool(path: &str) -> Option<&'static str> {
    match path_without_query(path) {
        "/api/notify" => Some("weave_notify"),
        "/api/ask" => Some("weave_ask"),
        "/api/answer" => Some("weave_answer"),
        "/api/reply" => Some("weave_reply"),
        "/api/job-cancel" => Some("weave_job_cancel"),
        "/api/job-create" => Some("weave_job_create"),
        "/api/turn-state" => Some("weave_set_turn_state"),
        "/api/description" => Some("weave_set_description"),
        "/api/spawn-peer" => Some("weave_spawn_peer"),
        "/api/kill-peer" => Some("weave_kill_peer"),
        _ => None,
    }
}

#[cfg(feature = "surfaces")]
fn dashboard_action_request(tool: &str, body: &[u8]) -> anyhow::Result<Value> {
    let fields = parse_form_urlencoded(std::str::from_utf8(body).unwrap_or_default());
    let mut args = serde_json::Map::new();
    for key in [
        "from",
        "to",
        "subject",
        "body",
        "correlation_id",
        "reply_to",
        "in_reply_to",
        "kind",
        "options",
        "priority",
        "job_id",
        "reason",
        "me",
        "state",
        "description",
        "creator",
        "title",
        "kind",
        "owner",
        "assignee",
        "circle",
        "prompt",
        "name",
        "cmd",
        "cwd",
        "mux",
        "window",
    ] {
        if let Some(value) = fields.get(key).filter(|v| !v.trim().is_empty()) {
            if tool == "weave_spawn_peer" && key == "cmd" {
                if let Ok(v @ Value::Array(_)) = serde_json::from_str::<Value>(value) {
                    args.insert(key.to_string(), v);
                    continue;
                }
            }
            if tool == "weave_spawn_peer" && key == "window" {
                if let Ok(v) = value.parse::<bool>() {
                    args.insert(key.to_string(), Value::Bool(v));
                    continue;
                }
            }
            if matches!(key, "in_reply_to" | "ttl" | "supersedes") {
                if let Ok(n) = value.parse::<i64>() {
                    args.insert(key.to_string(), Value::Number(n.into()));
                    continue;
                }
            }
            args.insert(key.to_string(), Value::String(value.clone()));
        }
    }
    Ok(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "dashboard-action",
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": Value::Object(args),
        }
    }))
}

#[cfg(feature = "surfaces")]
fn parse_form_urlencoded(body: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in body.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode_form(key), percent_decode_form(value));
    }
    out
}

#[cfg(feature = "surfaces")]
fn percent_decode_form(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(feature = "surfaces")]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Compose a read-only [`dashboard::DashboardSnapshot`] from existing `Store`
/// reads plus token-free runtime settings. No new SQL, no new trait method — just
/// the existing list/inbox calls.
#[cfg(feature = "surfaces")]
fn build_snapshot(
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
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
    let mut asks_by_id: std::collections::HashMap<String, weave_core::model::Ask> =
        std::collections::HashMap::new();
    for p in &peers {
        for ask in store
            .list_asks(&p.name, weave_core::model::AskRole::Any, 20)
            .unwrap_or_default()
        {
            asks_by_id.entry(ask.id.clone()).or_insert(ask);
        }
    }
    let mut asks: Vec<weave_core::model::Ask> = asks_by_id.into_values().collect();
    asks.sort_by(|a, b| b.updated_ts.cmp(&a.updated_ts).then(b.id.cmp(&a.id)));
    asks.truncate(50);
    let leases = store.list_leases(50).unwrap_or_default();
    let schedules = store.list_schedules("", 50).unwrap_or_default();
    Ok(crate::dashboard::DashboardSnapshot {
        peers,
        messages,
        jobs,
        asks,
        leases,
        schedules,
        settings: dashboard_settings(store, write_enabled)?,
    })
}

#[cfg(feature = "surfaces")]
struct DashboardBridgeStatus {
    configured: bool,
    ready: bool,
    active: bool,
    stale: bool,
    healthy: bool,
    status: String,
    runtime_present: bool,
    runtime_status: String,
    heartbeat: i64,
    identity: String,
    recipient: String,
    pending: i64,
    last_success: i64,
    last_delivery: i64,
    last_error_class: String,
    issues: Vec<String>,
}

#[cfg(feature = "surfaces")]
fn dashboard_bridge_status(
    store: &dyn weave_core::store::Store,
    view: weave_core::config::BridgeConfigView,
) -> anyhow::Result<DashboardBridgeStatus> {
    let runtime = store.bridge_runtime_status(view.platform)?;
    let now = weave_core::model::now();
    let active = runtime
        .as_ref()
        .is_some_and(|state| state.is_active_at(now));
    let stale = runtime.as_ref().is_some_and(|state| state.is_stale_at(now));
    let healthy = view.ready
        && active
        && runtime.as_ref().is_some_and(|state| {
            let external_route_matches =
                weave_core::model::BridgeCursorEnvelope::decode(&state.cursor)
                    .ok()
                    .flatten()
                    .is_some_and(|cursor| {
                        view.conversation.as_deref() == Some(cursor.external_scope.as_str())
                    });
            state.status == weave_core::model::BridgeRuntimeStatus::Running
                && state.last_error_class.is_empty()
                && view.identity.as_deref() == Some(state.identity.as_str())
                && view.recipient.as_deref() == Some(state.recipient.as_str())
                && external_route_matches
        });
    let status = if healthy {
        "healthy"
    } else if active {
        "degraded"
    } else if stale {
        "stale"
    } else if view.ready {
        "ready_inactive"
    } else if view.configured {
        "not_ready"
    } else {
        "not_configured"
    };
    let pending = match view.identity.as_deref() {
        Some(identity) => store.unread_count(identity)?,
        None => 0,
    };
    let mut issues = view
        .issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if runtime.as_ref().is_some_and(|state| {
        view.identity.as_deref() != Some(state.identity.as_str())
            || view.recipient.as_deref() != Some(state.recipient.as_str())
    }) {
        issues.push("runtime route differs from current configuration".to_string());
    }
    if runtime.as_ref().is_some_and(|state| {
        weave_core::model::BridgeCursorEnvelope::decode(&state.cursor)
            .ok()
            .flatten()
            .is_none_or(|cursor| {
                view.conversation.as_deref() != Some(cursor.external_scope.as_str())
            })
    }) {
        issues.push("runtime external route differs from current configuration".to_string());
    }
    Ok(DashboardBridgeStatus {
        configured: view.configured,
        ready: view.ready,
        active,
        stale,
        healthy,
        status: status.to_string(),
        runtime_present: runtime.is_some(),
        runtime_status: runtime
            .as_ref()
            .map(|state| state.status.as_str().to_string())
            .unwrap_or_else(|| "-".to_string()),
        heartbeat: runtime.as_ref().map_or(0, |state| state.heartbeat_ts),
        identity: view.identity.unwrap_or_else(|| "-".to_string()),
        recipient: view.recipient.unwrap_or_else(|| "-".to_string()),
        pending,
        last_success: runtime.as_ref().map_or(0, |state| state.last_success_ts),
        last_delivery: runtime.as_ref().map_or(0, |state| state.last_delivery_ts),
        last_error_class: runtime
            .as_ref()
            .map(|state| state.last_error_class.clone())
            .unwrap_or_default(),
        issues,
    })
}

#[cfg(feature = "surfaces")]
fn dashboard_settings(
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
) -> anyhow::Result<crate::dashboard::DashboardSettings> {
    let cfg = weave_core::config::Config::load();
    let telegram = dashboard_bridge_status(store, cfg.telegram_bridge_config_view())?;
    let slack = dashboard_bridge_status(store, cfg.slack_bridge_config_view())?;
    Ok(crate::dashboard::DashboardSettings {
        circle: cfg.circle(),
        write_enabled,
        spawn_allowed_dirs: cfg.spawn_allowed_dirs.clone().unwrap_or_default(),
        peer_db_count: cfg.peer_dbs.as_ref().map_or(0, Vec::len),
        pull_from_count: cfg.pull_from.as_ref().map_or(0, Vec::len),
        inject_pulled: cfg.inject_pulled(),
        allow_inject_from_count: cfg.allow_inject_from.as_ref().map(Vec::len),
        bridge_identity: cfg
            .bridge_identity
            .as_deref()
            .filter(|identity| {
                weave_core::model::validate_ident("legacy bridge identity", identity).is_ok()
            })
            .unwrap_or("-")
            .to_string(),
        telegram_configured: telegram.configured,
        telegram_ready: telegram.ready,
        telegram_active: telegram.active,
        telegram_stale: telegram.stale,
        telegram_healthy: telegram.healthy,
        telegram_status: telegram.status,
        telegram_runtime_present: telegram.runtime_present,
        telegram_runtime_status: telegram.runtime_status,
        telegram_heartbeat: telegram.heartbeat,
        telegram_identity: telegram.identity,
        telegram_recipient: telegram.recipient,
        telegram_pending: telegram.pending,
        telegram_last_success: telegram.last_success,
        telegram_last_delivery: telegram.last_delivery,
        telegram_last_error_class: telegram.last_error_class,
        telegram_issues: telegram.issues,
        slack_configured: slack.configured,
        slack_ready: slack.ready,
        slack_active: slack.active,
        slack_stale: slack.stale,
        slack_healthy: slack.healthy,
        slack_status: slack.status,
        slack_runtime_present: slack.runtime_present,
        slack_runtime_status: slack.runtime_status,
        slack_heartbeat: slack.heartbeat,
        slack_identity: slack.identity,
        slack_recipient: slack.recipient,
        slack_pending: slack.pending,
        slack_last_success: slack.last_success,
        slack_last_delivery: slack.last_delivery,
        slack_last_error_class: slack.last_error_class,
        slack_issues: slack.issues,
        pretooluse_approver_configured: cfg
            .pretooluse_approver
            .as_deref()
            .is_some_and(|s| !s.is_empty()),
        pretooluse_timeout_secs: cfg.pretooluse_timeout(),
        obscura_allow_ops: cfg.obscura_allow_ops.clone().unwrap_or_default(),
        obscura_allow_domains: cfg.obscura_allow_domains.clone().unwrap_or_default(),
        obscura_allow_internal: cfg.obscura_allow_internal.unwrap_or(false),
    })
}

#[cfg(feature = "surfaces")]
fn peer_json(p: &weave_core::model::Peer, now: i64) -> serde_json::Value {
    let live = now - p.last_seen <= 90;
    serde_json::json!({
        "peer_id": weave_core::model::peer_session_id(p),
        "name": p.name,
        "display_name": if p.description.is_empty() { p.name.as_str() } else { p.description.as_str() },
        "status": if live { "online" } else { "offline" },
        "turn_state": if p.turn_state.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(p.turn_state.clone()) },
        "machine": p.host,
        "path": p.cwd.as_deref().unwrap_or(""),
        "tmux_session": if p.mux == "tmux" { Some(p.target.as_str()) } else { None },
        "backend": p.mux,
        "model": serde_json::Value::Null,
        "circle": weave_core::model::circle_or_default(&p.circle),
        "role": if p.role == "orchestrator" { "orchestrator" } else { "agent" },
        "last_seen": weave_core::model::fmt_ts(p.last_seen),
        "description": p.description,
        "metadata": {
            "repo": p.repo,
            "branch": p.branch,
            "worktree": p.worktree_id,
            "mux": p.mux,
            "target": p.target,
            "session_id_basis": weave_core::model::peer_session_id_basis(p),
        }
    })
}

#[cfg(feature = "surfaces")]
fn event_json(m: &weave_core::model::Message) -> serde_json::Value {
    serde_json::json!({
        "id": format!("msg_{}", m.id),
        "entity": "message",
        "type": if m.recipient == "all" { "broadcast" } else { "notification" },
        "event_type": if m.recipient == "all" { "message.broadcast" } else { "message.notification" },
        "source_id": m.id,
        "timestamp": weave_core::model::fmt_ts(m.ts),
        "from": m.sender,
        "to": m.recipient,
        "from_peer_id": m.sender,
        "to_peer_id": m.recipient,
        "text": m.body,
        "status": "success",
        "delivered": true,
        "has_message": !m.body.is_empty(),
        "subject": m.subject,
        "priority": m.priority,
    })
}

#[cfg(feature = "surfaces")]
fn ask_event_json(a: &weave_core::model::Ask) -> serde_json::Value {
    serde_json::json!({
        "id": format!("ask_{}", a.id),
        "entity": "ask",
        "type": if a.state == weave_core::model::AskState::Open { "ask_open" } else { "ask_closed" },
        "event_type": format!("ask.{}", a.state.as_str()),
        "timestamp": weave_core::model::fmt_ts(a.updated_ts),
        "from": a.asker,
        "to": a.askee,
        "ask_id": a.id,
        "subject": a.subject,
        "text": a.subject.as_deref().unwrap_or("question"),
        "status": a.state.as_str(),
        "kind": a.kind.as_str(),
        "has_message": true,
    })
}

#[cfg(feature = "surfaces")]
fn job_event_json(j: &weave_core::model::Job) -> serde_json::Value {
    serde_json::json!({
        "id": format!("job_{}", j.id),
        "entity": "job",
        "type": format!("job_{}", j.state.as_str()),
        "event_type": format!("job.{}", j.state.as_str()),
        "timestamp": weave_core::model::fmt_ts(j.updated_ts),
        "job_id": j.id,
        "title": j.title,
        "text": if j.description.is_empty() { j.title.as_str() } else { j.description.as_str() },
        "status": j.state.as_str(),
        "phase": j.phase,
        "from": j.creator,
        "to": j.assignee.as_deref().unwrap_or(""),
        "has_message": !j.description.is_empty(),
    })
}

#[cfg(feature = "surfaces")]
fn peer_event_json(p: &weave_core::model::Peer, now: i64) -> serde_json::Value {
    let live = now - p.last_seen <= 90;
    serde_json::json!({
        "id": format!("peer_{}", weave_core::model::peer_session_id(p)),
        "entity": "peer",
        "type": if live { "peer_online" } else { "peer_offline" },
        "event_type": if live { "peer.online" } else { "peer.offline" },
        "timestamp": weave_core::model::fmt_ts(p.last_seen),
        "peer_id": weave_core::model::peer_session_id(p),
        "name": p.name,
        "status": if live { "online" } else { "offline" },
        "repo": p.repo,
        "branch": p.branch,
        "has_message": false,
    })
}

#[cfg(feature = "surfaces")]
fn mesh_events_json(
    snap: &crate::dashboard::DashboardSnapshot,
    now: i64,
) -> Vec<serde_json::Value> {
    let mut events: Vec<(i64, serde_json::Value)> = Vec::new();
    events.extend(snap.messages.iter().map(|m| (m.ts, event_json(m))));
    events.extend(snap.asks.iter().map(|a| (a.updated_ts, ask_event_json(a))));
    events.extend(snap.jobs.iter().map(|j| (j.updated_ts, job_event_json(j))));
    events.extend(
        snap.peers
            .iter()
            .map(|p| (p.last_seen, peer_event_json(p, now))),
    );
    events.sort_by_key(|event| std::cmp::Reverse(event.0));
    events
        .into_iter()
        .take(100)
        .map(|(_, event)| event)
        .collect()
}

#[cfg(feature = "surfaces")]
fn job_summary_json(j: &weave_core::model::Job) -> serde_json::Value {
    serde_json::json!({
        "job_id": j.id,
        "work_id": j.id,
        "title": j.title,
        "kind": j.kind,
        "state": j.state.as_str(),
        "state_reason": j.state_reason,
        "phase": j.phase,
        "progress_events": serde_json::from_str::<serde_json::Value>(&j.progress_events_json).unwrap_or_else(|_| serde_json::json!([])),
        "owner_peer_id": j.owner,
        "assigned_peer_id": j.assignee,
        "correlation_id": j.correlation_id,
        "circle": j.circle,
        "created_by_peer_id": j.creator,
        "source_kind": j.source_kind,
        "source_id": j.source_id,
        "scope": j.scope,
        "visibility": j.visibility,
        "created_at": weave_core::model::fmt_ts(j.opened_ts),
        "updated_at": weave_core::model::fmt_ts(j.updated_ts),
        "deadline_at": j.deadline_at.map(weave_core::model::fmt_ts),
        "expires_at": j.expires_at.map(weave_core::model::fmt_ts),
        "result_summary": j.result_summary,
        "cancel_requested": j.cancel_requested,
        "cancellation_reason": j.cancel_reason,
    })
}

#[cfg(feature = "surfaces")]
fn ask_json(a: &weave_core::model::Ask) -> serde_json::Value {
    serde_json::json!({
        "ask_id": a.id,
        "id": a.id,
        "question_msg_id": a.question_msg_id,
        "answer_msg_id": a.answer_msg_id,
        "asker_peer_id": a.asker,
        "askee_peer_id": a.askee,
        "asker": a.asker,
        "askee": a.askee,
        "subject": a.subject,
        "state": a.state.as_str(),
        "kind": a.kind.as_str(),
        "options": a.options,
        "reply_to": a.reply_to,
        "parent_id": a.parent_id,
        "opened_at": weave_core::model::fmt_ts(a.opened_ts),
        "updated_at": weave_core::model::fmt_ts(a.updated_ts),
        "closed_at": a.closed_ts.map(weave_core::model::fmt_ts),
    })
}

#[cfg(feature = "surfaces")]
fn transcript_peer_from_path(path: &str) -> Option<&str> {
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    let rest = path.strip_prefix("/peers/")?;
    let peer = rest.strip_suffix("/transcript")?;
    if peer.is_empty() || peer.contains('/') {
        return None;
    }
    Some(peer)
}

#[cfg(feature = "surfaces")]
fn job_status_id_from_path(path: &str) -> Option<&str> {
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    let rest = path.strip_prefix("/jobs/")?;
    let job_id = rest.strip_suffix("/status")?;
    if job_id.is_empty() || job_id.contains('/') {
        return None;
    }
    Some(job_id)
}

#[cfg(feature = "surfaces")]
fn job_result_id_from_path(path: &str) -> Option<&str> {
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    let rest = path.strip_prefix("/jobs/")?;
    let job_id = rest.strip_suffix("/result")?;
    if job_id.is_empty() || job_id.contains('/') {
        return None;
    }
    Some(job_id)
}

#[cfg(feature = "surfaces")]
fn dashboard_snapshot_json(
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
) -> anyhow::Result<serde_json::Value> {
    let snap = build_snapshot(store, write_enabled)?;
    let now = weave_core::model::now();
    Ok(serde_json::json!({
        "schema": "weave.dashboard.v1",
        "source": "weave-rust-surfaces",
        "repowire_compat": true,
        "generated_at": weave_core::model::fmt_ts(now),
        "peers": snap.peers.iter().map(|p| peer_json(p, now)).collect::<Vec<_>>(),
        "events": mesh_events_json(&snap, now),
        "asks": snap.asks.iter().map(ask_json).collect::<Vec<_>>(),
        "pending_questions": snap.asks.iter().filter(|a| a.state == weave_core::model::AskState::Open).map(ask_json).collect::<Vec<_>>(),
        "jobs": {
            "work": snap.jobs.iter().map(job_summary_json).collect::<Vec<_>>(),
            "recurring": [],
        },
        "leases": snap.leases,
        "schedules": snap.schedules,
        "settings": settings_json(&snap.settings),
    }))
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_snapshot_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
) -> anyhow::Result<()> {
    write_http_json(stream, &dashboard_snapshot_json(store, write_enabled)?)?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_peers_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, false)?;
    let now = weave_core::model::now();
    write_http_json(
        stream,
        &serde_json::json!({"peers": snap.peers.iter().map(|p| peer_json(p, now)).collect::<Vec<_>>()}),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn query_param<'a>(path: &'a str, wanted: &str) -> Option<&'a str> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == wanted).then_some(v)
    })
}

#[cfg(feature = "surfaces")]
fn parse_event_since(raw: &str) -> Option<i64> {
    raw.strip_prefix("msg_").unwrap_or(raw).parse().ok()
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_events_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    path: &str,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, false)?;
    let since = query_param(path, "since").and_then(parse_event_since);
    let events = if let Some(since) = since {
        snap.messages
            .iter()
            .filter(|m| m.id > since)
            .map(event_json)
            .collect::<Vec<_>>()
    } else {
        mesh_events_json(&snap, weave_core::model::now())
    };
    write_http_json(
        stream,
        &serde_json::json!({
            "events": events,
            "next_since": snap.messages.iter().map(|m| m.id).max(),
        }),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_jobs_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, false)?;
    write_http_json(
        stream,
        &serde_json::json!({"work": snap.jobs.iter().map(job_summary_json).collect::<Vec<_>>(), "recurring": []}),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_asks_pending_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, false)?;
    write_http_json(
        stream,
        &serde_json::json!({
            "pending_questions": snap
                .asks
                .iter()
                .filter(|a| a.state == weave_core::model::AskState::Open)
                .map(ask_json)
                .collect::<Vec<_>>()
        }),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_peer_transcript_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    peer: &str,
    path: &str,
) -> anyhow::Result<()> {
    let before = query_param(path, "before").and_then(parse_event_since);
    let query = query_param(path, "q")
        .map(percent_decode_form)
        .map(|q| q.to_ascii_lowercase());
    let turns = store.history(peer, None, 50).unwrap_or_default();
    let filtered = turns
        .iter()
        .filter(|m| before.is_none_or(|id| m.id < id))
        .filter(|m| {
            query.as_ref().is_none_or(|q| {
                m.body.to_ascii_lowercase().contains(q)
                    || m.subject
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(q)
                    || m.sender.to_ascii_lowercase().contains(q)
                    || m.recipient.to_ascii_lowercase().contains(q)
            })
        })
        .collect::<Vec<_>>();
    let next_before = filtered.iter().map(|m| m.id).min();
    write_http_json(
        stream,
        &serde_json::json!({
            "peer_id": peer,
            "turns": filtered.iter().map(|m| event_json(m)).collect::<Vec<_>>(),
            "next_before": next_before,
            "query": query,
        }),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_job_status_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    job_id: &str,
) -> anyhow::Result<()> {
    match store.get_job(job_id)? {
        Some(job) => write_http_json(stream, &serde_json::json!({"job": job_summary_json(&job)}))?,
        None => write_http(stream, 404, b"Not Found")?,
    }
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_job_result_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    job_id: &str,
) -> anyhow::Result<()> {
    match store.job_result(job_id)? {
        Some(result) => write_http_json(stream, &serde_json::json!({"result": result}))?,
        None => write_http(stream, 404, b"Not Found")?,
    }
    Ok(())
}

#[cfg(feature = "surfaces")]
fn settings_json(s: &crate::dashboard::DashboardSettings) -> serde_json::Value {
    serde_json::json!({
        "circle": &s.circle,
        "write_enabled": s.write_enabled,
        "spawn_allowed_dirs": &s.spawn_allowed_dirs,
        "peer_db_count": s.peer_db_count,
        "pull_from_count": s.pull_from_count,
        "inject_pulled": s.inject_pulled,
        "allow_inject_from_count": s.allow_inject_from_count,
        "bridge_identity": &s.bridge_identity,
        "telegram_configured": s.telegram_configured,
        "telegram_ready": s.telegram_ready,
        "telegram_active": s.telegram_active,
        "telegram_stale": s.telegram_stale,
        "telegram_healthy": s.telegram_healthy,
        "telegram_status": &s.telegram_status,
        "telegram_runtime_present": s.telegram_runtime_present,
        "telegram_runtime_status": &s.telegram_runtime_status,
        "telegram_heartbeat": s.telegram_heartbeat,
        "telegram_identity": &s.telegram_identity,
        "telegram_recipient": &s.telegram_recipient,
        "telegram_pending": s.telegram_pending,
        "telegram_last_success": s.telegram_last_success,
        "telegram_last_delivery": s.telegram_last_delivery,
        "telegram_last_error_class": &s.telegram_last_error_class,
        "telegram_issues": &s.telegram_issues,
        "slack_configured": s.slack_configured,
        "slack_ready": s.slack_ready,
        "slack_active": s.slack_active,
        "slack_stale": s.slack_stale,
        "slack_healthy": s.slack_healthy,
        "slack_status": &s.slack_status,
        "slack_runtime_present": s.slack_runtime_present,
        "slack_runtime_status": &s.slack_runtime_status,
        "slack_heartbeat": s.slack_heartbeat,
        "slack_identity": &s.slack_identity,
        "slack_recipient": &s.slack_recipient,
        "slack_pending": s.slack_pending,
        "slack_last_success": s.slack_last_success,
        "slack_last_delivery": s.slack_last_delivery,
        "slack_last_error_class": &s.slack_last_error_class,
        "slack_issues": &s.slack_issues,
        "pretooluse_approver_configured": s.pretooluse_approver_configured,
        "pretooluse_timeout_secs": s.pretooluse_timeout_secs,
        "obscura_allow_ops": &s.obscura_allow_ops,
        "obscura_allow_domains": &s.obscura_allow_domains,
        "obscura_allow_internal": s.obscura_allow_internal,
    })
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_settings_json(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, write_enabled)?;
    write_http_json(
        stream,
        &serde_json::json!({"settings": settings_json(&snap.settings)}),
    )?;
    Ok(())
}

#[cfg(feature = "surfaces")]
fn serve_dashboard_health_json(stream: &mut TcpStream) -> anyhow::Result<()> {
    write_http_json(
        stream,
        &serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "surface": "dashboard",
            "repowire_compat": true,
        }),
    )?;
    Ok(())
}

/// `GET /` → render the dashboard HTML page once and close.
#[cfg(feature = "surfaces")]
fn serve_dashboard_page(
    stream: &mut TcpStream,
    store: &dyn weave_core::store::Store,
    write_enabled: bool,
) -> anyhow::Result<()> {
    let snap = build_snapshot(store, write_enabled)?;
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
    write_enabled: bool,
) -> anyhow::Result<()> {
    let header = format!(
        "{DEFAULT_PROTOCOL} 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return Ok(());
    }
    let host = weave_core::config::this_host();
    loop {
        let snap = build_snapshot(store, write_enabled)?;
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
    use super::{
        is_loopback_bind, read_bounded_body, read_bounded_header_line, read_bounded_line,
        validate_separate_push_token, MAX_HEADER_BYTES, MAX_HTTP_BODY_BYTES,
    };
    use std::io::{BufReader, Cursor};

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

    #[test]
    fn push_credential_must_be_nonempty_and_separate() {
        assert!(validate_separate_push_token("operator", None).is_ok());
        assert!(validate_separate_push_token("operator", Some("push-only")).is_ok());
        assert!(validate_separate_push_token("operator", Some("")).is_err());
        assert!(validate_separate_push_token("same", Some("same")).is_err());
    }

    #[test]
    fn request_lines_headers_and_bodies_are_hard_bounded() {
        let oversized_line = format!("{}\n", "x".repeat(33));
        let mut reader = BufReader::new(Cursor::new(oversized_line.into_bytes()));
        let mut line = String::new();
        assert!(read_bounded_line(&mut reader, &mut line, 32).is_err());

        let header = format!("X-Test: {}\r\n", "x".repeat(MAX_HEADER_BYTES));
        let mut reader = BufReader::new(Cursor::new(header.into_bytes()));
        let mut total = 0usize;
        let mut count = 0usize;
        let started = std::time::Instant::now();
        assert!(
            read_bounded_header_line(&mut reader, &mut line, &mut total, &mut count, &started,)
                .is_err()
        );

        let mut empty = Cursor::new(Vec::<u8>::new());
        assert!(read_bounded_body(&mut empty, MAX_HTTP_BODY_BYTES + 1).is_err());
        let mut short = Cursor::new(vec![0u8; 3]);
        assert!(read_bounded_body(&mut short, 4).is_err());
    }
}
