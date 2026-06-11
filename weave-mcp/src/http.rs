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

/// Start a blocking HTTP server on `127.0.0.1:port`. Only POST / is accepted.
/// Bearer token is required unless `token` is empty. Dangerous tools are
/// filtered unless `dangerous` is true.
#[allow(clippy::too_many_arguments)]
pub fn serve_http(
    store: &dyn weave_core::store::Store,
    me_default: Option<String>,
    nudge_template: Option<&str>,
    extra_dbs: Vec<StoreSource>,
    pull: PullConsent,
    injector: &dyn Injector,
    port: u16,
    token: &str,
    dangerous: bool,
) -> anyhow::Result<()> {
    let addr = format!("127.0.0.1:{port}");
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

    // Only POST / is accepted.
    if !first_line.starts_with("POST /") {
        write_http(stream, 405, b"Method Not Allowed")?;
        return Ok(());
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
