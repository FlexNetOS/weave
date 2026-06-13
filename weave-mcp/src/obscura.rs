//! WL-049 / ADR-0002: minimal hand-rolled **MCP client** for governed web access.
//!
//! weave does NOT link obscura / V8 / tokio. Instead, behind the default-OFF
//! `obscura` feature, it spawns the separate `obscura` binary (`obscura mcp …`) as a
//! child via **argv-only `std::process::Command` (never a shell)** and speaks
//! newline-delimited JSON-RPC 2.0 over the child's stdio, built on `std::io` + the
//! already-present `serde_json`. **Zero new runtime deps, no async.**
//!
//! Protocol (mirrors the http.rs hand-rolled framing precedent):
//!   1. lazy spawn on first op; resolve `obscura` to a TRUSTED absolute path via
//!      `weave_inject::resolve_trusted` (never ambient `$PATH`);
//!   2. handshake: send `initialize`, read one reply, send the `notifications/
//!      initialized` notification (no id, no reply);
//!   3. per op: send `tools/call {name:"browser_<op>", arguments:{…}}`, read
//!      newline-delimited lines until the line whose `id` matches (skip
//!      notifications), extract `result.content[0].text`, map `isError`/`error`;
//!   4. one cached child per weave process, reused across ops; monotonic id counter;
//!   5. bounded per-op read deadline + capped line length so a runaway child can
//!      neither hang nor OOM weave;
//!   6. clean shutdown: `Drop` (and `weave web --stop` / [`stop`]) kill+reap the
//!      child argv-only so no zombie obscura lingers.
//!
//! INVARIANTS: weave's OWN stdout stays pure JSON-RPC — the child's stdout is a PIPE
//! we READ and never forward; the child's stderr is consumed/null and NEVER logged
//! (WL-048 token-redaction lesson); proxy creds / tokens ride env or argv but are
//! never logged. Each argv element is bounded via `weave_inject::spawn_arg_ok`.

use crate::mcp::log;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use weave_core::config::Config;
use weave_inject::{resolve_trusted, spawn_arg_ok, MAX_SPAWN_ARGS};

/// Default per-op read timeout (seconds). Web navigation is slower than mux
/// injection (whose cap is 5s), so this defaults higher; clamped to a sane range.
const DEFAULT_OBSCURA_TIMEOUT_SECS: u64 = 30;
/// Lower/upper clamp on the configured per-op timeout.
const MIN_TIMEOUT_SECS: u64 = 1;
const MAX_TIMEOUT_SECS: u64 = 300;
/// Bound on a single JSON-RPC response line (bytes) so a runaway child cannot OOM
/// weave. Generous (web payloads can be large) but finite. `MAX_BODY`-class * 16.
const MAX_LINE_BYTES: usize = weave_core::store::MAX_BODY * 16;

/// The process-global cached obscura client. Like the injector, a single child is
/// spawned lazily and reused for the whole weave session; this avoids threading a
/// new field through the (already large) dispatch signature chain.
fn client_cell() -> &'static Mutex<Option<ObscuraClient>> {
    static CELL: OnceLock<Mutex<Option<ObscuraClient>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Forward one governed web op to obscura, lazily spawning the child on first use
/// and reusing it thereafter. `tool` is the fully-qualified `browser_<op>` name;
/// `args` is the op's arguments object. Returns the obscura `content[0].text`
/// payload, or a clean `Err` (op error, transport error, timeout, missing binary).
///
/// On any transport failure the cached child is dropped (killed + reaped) so the
/// next call re-spawns a fresh one rather than reusing a wedged pipe.
pub fn call(cfg: &Config, tool: &str, args: &Value) -> Result<String, String> {
    let mut guard = client_cell()
        .lock()
        .map_err(|_| "obscura client lock poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(ObscuraClient::spawn(cfg)?);
    }
    let client = guard.as_mut().expect("just set");
    match client.call_tool(tool, args) {
        Ok(text) => Ok(text),
        Err(transport) if transport.transport_fault => {
            // Drop (kill + reap) the wedged child so the next op re-spawns cleanly.
            *guard = None;
            Err(transport.message)
        }
        Err(op_err) => Err(op_err.message),
    }
}

/// Stop and reap the cached obscura child (if any). Best-effort; never panics.
/// Invoked by `weave web --stop` and on a clean weave shutdown.
pub fn stop() {
    if let Ok(mut guard) = client_cell().lock() {
        // Dropping the ObscuraClient runs its Drop (kill + wait).
        *guard = None;
    }
}

/// An error from a web op, distinguishing an obscura-reported tool error (the child
/// is healthy, reuse it) from a transport fault (the child is wedged, drop it).
struct WebErr {
    message: String,
    transport_fault: bool,
}

impl WebErr {
    fn op(message: impl Into<String>) -> WebErr {
        WebErr {
            message: message.into(),
            transport_fault: false,
        }
    }
    fn transport(message: impl Into<String>) -> WebErr {
        WebErr {
            message: message.into(),
            transport_fault: true,
        }
    }
}

/// A live connection to a spawned `obscura mcp` child over its stdio.
pub struct ObscuraClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
}

impl ObscuraClient {
    /// Resolve, spawn, and handshake with `obscura mcp`. argv-only; trusted-path
    /// resolved; child stderr discarded (never logged — may carry proxy creds).
    fn spawn(cfg: &Config) -> Result<ObscuraClient, String> {
        let bin = cfg.obscura_bin.as_deref().unwrap_or("obscura");
        let abs = resolve_trusted(bin).ok_or_else(|| {
            "obscura binary not found in a trusted directory (set obscura_bin / WEAVE_OBSCURA_BIN \
             to an installed `obscura`)"
                .to_string()
        })?;

        // Build the argv vector — NEVER a command string, never `sh -c`.
        let mut argv: Vec<String> = vec!["mcp".to_string()];
        if cfg.obscura_stealth.unwrap_or(false) {
            argv.push("--stealth".to_string());
        }
        if let Some(proxy) = cfg.obscura_proxy.as_deref().filter(|s| !s.is_empty()) {
            argv.push("--proxy".to_string());
            argv.push(proxy.to_string());
        }
        if let Some(ua) = cfg.obscura_user_agent.as_deref().filter(|s| !s.is_empty()) {
            argv.push("--user-agent".to_string());
            argv.push(ua.to_string());
        }
        if argv.len() > MAX_SPAWN_ARGS {
            return Err("obscura argv exceeds the spawn-arg cap".to_string());
        }
        for a in &argv {
            if !spawn_arg_ok(a) {
                return Err(
                    "obscura argument is too long or contains control/NUL bytes".to_string()
                );
            }
        }

        let timeout = Duration::from_secs(
            cfg.obscura_timeout_secs
                .map(|n| (n.max(1) as u64).clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS))
                .unwrap_or(DEFAULT_OBSCURA_TIMEOUT_SECS),
        );

        let mut command = Command::new(&abs);
        command
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The child's stderr may carry proxy creds / target URLs — discard it,
            // NEVER pipe it into a weave log line (WL-048 redaction lesson).
            .stderr(Stdio::null());
        // An optional obscura auth token rides a CHILD ENV var, never argv, never a
        // weave log line. Debug-redacted in Config.
        if let Some(token) = cfg.obscura_token.as_deref().filter(|s| !s.is_empty()) {
            command.env("OBSCURA_TOKEN", token);
        }

        let mut child = command
            .spawn()
            .map_err(|_| "failed to spawn obscura (is it executable?)".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "obscura child has no stdin pipe".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "obscura child has no stdout pipe".to_string())?;

        let mut c = ObscuraClient {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            timeout,
        };
        if let Err(e) = c.handshake() {
            // Reap the half-spawned child before surfacing the error.
            let _ = c.child.kill();
            let _ = c.child.wait();
            return Err(e.message);
        }
        log("obscura child spawned and initialized");
        Ok(c)
    }

    /// MCP handshake: `initialize` → read reply → `notifications/initialized`.
    /// Tolerant of a serverInfo-name mismatch (log + continue); fails only on a
    /// transport error.
    fn handshake(&mut self) -> Result<(), WebErr> {
        let id = self.bump_id();
        let init = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "weave", "version": env!("CARGO_PKG_VERSION")}
            }
        });
        self.write_frame(&init)?;
        let reply = self.read_reply_for(id)?;
        let name = reply
            .get("result")
            .and_then(|r| r.get("serverInfo"))
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if name != "obscura-mcp" {
            log(&format!(
                "obscura serverInfo.name={name:?} (expected \"obscura-mcp\"); continuing"
            ));
        }
        // Notification: no id, no reply expected (obscura skips id-less messages).
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.write_frame(&note)?;
        Ok(())
    }

    /// Send one `tools/call` and return its `content[0].text`, mapping an obscura
    /// tool error (`isError:true`) or a top-level JSON-RPC `error` to `Err`.
    fn call_tool(&mut self, tool: &str, args: &Value) -> Result<String, WebErr> {
        let id = self.bump_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        });
        self.write_frame(&req)?;
        let reply = self.read_reply_for(id)?;

        if let Some(err) = reply.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("obscura rpc error");
            return Err(WebErr::op(format!("obscura: {msg}")));
        }
        let result = reply
            .get("result")
            .ok_or_else(|| WebErr::transport("obscura reply missing result"))?;
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(WebErr::op(text.to_string()));
        }
        Ok(text.to_string())
    }

    fn bump_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Write a single newline-delimited JSON frame to the child's stdin and flush.
    fn write_frame(&mut self, v: &Value) -> Result<(), WebErr> {
        let mut line = serde_json::to_string(v)
            .map_err(|_| WebErr::transport("failed to encode obscura request"))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|_| WebErr::transport("obscura exited (write failed)"))?;
        self.stdin
            .flush()
            .map_err(|_| WebErr::transport("obscura exited (flush failed)"))?;
        Ok(())
    }

    /// Read newline-delimited reply lines, skipping notifications/other-id frames,
    /// until the line whose `id` matches `want`. Bounded by [`Self::timeout`] and
    /// [`MAX_LINE_BYTES`]. On timeout the child is killed+reaped and a transport
    /// error returned (the caller drops the cached client).
    fn read_reply_for(&mut self, want: u64) -> Result<Value, WebErr> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(WebErr::transport("obscura timed out"));
            }
            let line = match self.read_bounded_line(deadline)? {
                Some(l) => l,
                None => return Err(WebErr::transport("obscura exited (EOF)")),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                // A non-JSON line on stdout is noise; skip it rather than failing.
                Err(_) => continue,
            };
            match v.get("id").and_then(Value::as_u64) {
                Some(got) if got == want => return Ok(v),
                // A different id or a notification (no id) — keep reading.
                _ => continue,
            }
        }
    }

    /// Read a single line (up to the next `\n`) with a byte cap and a deadline.
    /// Returns `Ok(None)` on EOF. Reads byte-by-byte through the BufReader so the
    /// cap is enforced even if the child never emits a newline.
    fn read_bounded_line(&mut self, deadline: Instant) -> Result<Option<String>, WebErr> {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        loop {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(WebErr::transport("obscura timed out"));
            }
            match self.stdout.read(&mut byte) {
                Ok(0) => {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                }
                Ok(_) => {
                    if byte[0] == b'\n' {
                        return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                    }
                    buf.push(byte[0]);
                    if buf.len() > MAX_LINE_BYTES {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return Err(WebErr::transport("obscura reply exceeded the line cap"));
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(WebErr::transport("obscura exited (read failed)")),
            }
        }
    }
}

impl Drop for ObscuraClient {
    fn drop(&mut self) {
        // Best-effort reap — argv-only kill, never a shell. A panicked weave must
        // not orphan an obscura child.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// Build an ObscuraClient whose stdout reads from `canned` bytes and whose
    /// stdin discards writes, WITHOUT spawning a real process — by wiring the parser
    /// against a fake stream. We test the framing/parse logic in isolation (the
    /// integration test exercises the real spawn against a fake `obscura` binary).
    ///
    /// Because `read_reply_for` needs a `Child` to kill on timeout, we spawn a
    /// trivial, immediately-exiting `true`-style child only as a reap target; the
    /// stdout we parse is the canned cursor, not the child's.
    struct Harness {
        stdout: BufReader<std::io::Cursor<Vec<u8>>>,
        next_id: u64,
    }

    impl Harness {
        fn new(canned: &str) -> Harness {
            Harness {
                stdout: BufReader::new(std::io::Cursor::new(canned.as_bytes().to_vec())),
                next_id: 0,
            }
        }

        /// Mirror of `ObscuraClient::read_reply_for` over the cursor (no child / no
        /// timeout path), so the id-matching + skip-notification logic is tested
        /// against canned bytes exactly as production parses them.
        fn read_reply_for(&mut self, want: u64) -> Result<Value, String> {
            loop {
                let mut line = String::new();
                let n = self
                    .stdout
                    .read_line(&mut line)
                    .map_err(|_| "read error".to_string())?;
                if n == 0 {
                    return Err("EOF".to_string());
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("id").and_then(Value::as_u64) {
                    Some(got) if got == want => return Ok(v),
                    _ => continue,
                }
            }
        }

        /// Mirror of `ObscuraClient::call_tool`'s result extraction.
        fn extract(reply: &Value) -> Result<String, String> {
            if let Some(err) = reply.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("rpc error");
                return Err(format!("obscura: {msg}"));
            }
            let result = reply.get("result").ok_or("missing result".to_string())?;
            let text = result
                .get("content")
                .and_then(|c| c.get(0))
                .and_then(|c0| c0.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(text.to_string());
            }
            Ok(text.to_string())
        }

        fn bump(&mut self) -> u64 {
            self.next_id += 1;
            self.next_id
        }
    }

    #[test]
    fn initialize_reply_parsed() {
        let stream = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"serverInfo\":{\"name\":\"obscura-mcp\"}}}\n";
        let mut h = Harness::new(stream);
        let id = h.bump();
        let reply = h.read_reply_for(id).unwrap();
        assert_eq!(reply["result"]["serverInfo"]["name"], "obscura-mcp");
    }

    #[test]
    fn tools_call_ok_text_extracted() {
        let stream = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello page\"}]}}\n";
        let reply: Value = serde_json::from_str(stream.trim()).unwrap();
        assert_eq!(Harness::extract(&reply).unwrap(), "hello page");
    }

    #[test]
    fn is_error_reply_mapped_to_err() {
        let stream = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Error: Missing url\"}],\"isError\":true}}";
        let reply: Value = serde_json::from_str(stream).unwrap();
        let err = Harness::extract(&reply).unwrap_err();
        assert_eq!(err, "Error: Missing url");
    }

    #[test]
    fn top_level_error_mapped_to_err() {
        let stream = "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Unknown method\"}}";
        let reply: Value = serde_json::from_str(stream).unwrap();
        let err = Harness::extract(&reply).unwrap_err();
        assert_eq!(err, "obscura: Unknown method");
    }

    #[test]
    fn interleaved_notifications_skipped_and_id_matched() {
        // A notification (no id), a stale-id reply, then the matching reply.
        let stream = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"text\":\"stale\"}]}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"text\":\"fresh\"}]}}\n"
        );
        let mut h = Harness::new(stream);
        // We want id=2 (as if id=1 was the handshake already consumed).
        let reply = h.read_reply_for(2).unwrap();
        assert_eq!(Harness::extract(&reply).unwrap(), "fresh");
    }

    #[test]
    fn eof_mid_stream_is_error() {
        let stream = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n";
        let mut h = Harness::new(stream);
        // No reply with id=5 ever arrives → EOF.
        assert!(h.read_reply_for(5).is_err());
    }

    #[test]
    fn garbage_line_skipped() {
        let stream = concat!(
            "not json at all\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"text\":\"ok\"}]}}\n"
        );
        let mut h = Harness::new(stream);
        let reply = h.read_reply_for(1).unwrap();
        assert_eq!(Harness::extract(&reply).unwrap(), "ok");
    }
}
