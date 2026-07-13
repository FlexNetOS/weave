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
//!      `weave_inject::resolve_trusted_program` (never ambient `$PATH`);
//!   2. handshake: send `initialize`, read one reply, send the `notifications/
//!      initialized` notification (no id, no reply);
//!   3. per op: send `tools/call {name:"browser_<op>", arguments:{…}}`, read
//!      newline-delimited lines until the line whose `id` matches (skip
//!      notifications), extract `result.content[0].text`, map `isError`/`error`;
//!   4. one cached child per weave process, reused across ops; monotonic id counter;
//!   5. bounded per-op read/write deadlines + capped frame/line lengths so a
//!      runaway child can neither hang nor OOM weave;
//!   6. clean shutdown: `Drop` (and `weave web --stop` / [`stop`]) kill+reap the
//!      child argv-only so no zombie obscura lingers.
//!
//! INVARIANTS: weave's OWN stdout stays pure JSON-RPC — the child's stdout is a PIPE
//! we READ and never forward; the child's stderr is consumed/null and NEVER logged
//! (WL-048 redaction lesson). The child environment is scrubbed except for `PATH`
//! and a validated writable-location hint from `TMPDIR`; proxy URLs containing
//! credentials are rejected because upstream accepts MCP proxy configuration only
//! through argv. Each argv element is bounded via
//! `weave_inject::spawn_arg_ok`.

use crate::mcp::log;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use weave_core::config::Config;
use weave_inject::{resolve_trusted_program, spawn_arg_ok, MAX_SPAWN_ARGS};

/// Default per-op read timeout (seconds). Web navigation is slower than mux
/// injection (whose cap is 5s), so this defaults higher; clamped to a sane range.
const DEFAULT_OBSCURA_TIMEOUT_SECS: u64 = 30;
/// Lower/upper clamp on the configured per-op timeout.
const MIN_TIMEOUT_SECS: u64 = 1;
const MAX_TIMEOUT_SECS: u64 = 300;
/// Bound on a single JSON-RPC response line (bytes) so a runaway child cannot OOM
/// weave. Generous (web payloads can be large) but finite. `MAX_BODY`-class * 16.
const MAX_LINE_BYTES: usize = weave_core::store::MAX_BODY * 16;
/// Bound outgoing protocol frames independently of webpolicy so initialization and
/// future call sites can never enqueue an unbounded write to the child.
const MAX_FRAME_BYTES: usize = weave_core::store::MAX_BODY * 16;
const READER_QUEUE_DEPTH: usize = 8;

enum ReaderEvent {
    Line(String),
    Eof,
    Error(String),
}

struct WriteRequest {
    frame: Vec<u8>,
    acknowledgement: mpsc::SyncSender<Result<(), String>>,
}

/// Own the blocking stdin pipe on a dedicated writer thread. A child that keeps
/// stdin open without reading can fill the OS pipe; the request thread therefore
/// waits for an acknowledgement with the same bounded operation timeout used for
/// replies. Killing the child closes the pipe and releases a blocked writer.
fn spawn_stdin_writer(mut stdin: ChildStdin) -> mpsc::Sender<WriteRequest> {
    let (sender, receiver) = mpsc::channel::<WriteRequest>();
    std::thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            let result = stdin
                .write_all(&request.frame)
                .and_then(|_| stdin.flush())
                .map_err(|_| "obscura exited (write failed)".to_string());
            let failed = result.is_err();
            if request.acknowledgement.send(result).is_err() || failed {
                break;
            }
        }
    });
    sender
}

fn validate_proxy_arg(proxy: &str) -> Result<(), String> {
    let parsed = url::Url::parse(proxy)
        .map_err(|_| "obscura proxy must be a valid HTTP(S) or SOCKS5 URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")
        || parsed.host_str().is_none()
    {
        return Err("obscura proxy must be a valid HTTP(S) or SOCKS5 URL".to_string());
    }
    let origin_only = matches!(parsed.path(), "" | "/")
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if !parsed.username().is_empty() || parsed.password().is_some() || !origin_only {
        return Err(
            "obscura proxy must be an origin-only credential-free URL (no userinfo, path, query, or fragment) because the upstream MCP CLI exposes it in the process list"
                .to_string(),
        );
    }
    Ok(())
}

fn encode_frame(value: &Value) -> Result<Vec<u8>, WebErr> {
    let mut frame = serde_json::to_vec(value)
        .map_err(|_| WebErr::transport("failed to encode obscura request"))?;
    if frame.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(WebErr::transport("obscura request exceeded the frame cap"));
    }
    frame.push(b'\n');
    Ok(frame)
}

/// Own the blocking stdout pipe on a dedicated reader thread. The request thread
/// waits on the bounded channel with `recv_timeout`, so a child that remains alive
/// but never emits a byte cannot bypass the operation deadline.
fn spawn_stdout_reader(stdout: ChildStdout) -> mpsc::Receiver<ReaderEvent> {
    let (sender, receiver) = mpsc::sync_channel(READER_QUEUE_DEPTH);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let event = match read_capped_line_from(&mut reader, MAX_LINE_BYTES) {
                Ok(Some(line)) => ReaderEvent::Line(line),
                Ok(None) => ReaderEvent::Eof,
                Err(error) => ReaderEvent::Error(error.message),
            };
            let terminal = matches!(event, ReaderEvent::Eof | ReaderEvent::Error(_));
            if sender.send(event).is_err() || terminal {
                break;
            }
        }
    });
    receiver
}

/// Every value fixed at child-spawn time. The web policy itself is evaluated on
/// every call, but a cached child must be replaced whenever one of these settings
/// changes or the two enforcement layers can disagree.
#[derive(Clone, PartialEq, Eq)]
struct SpawnSettings {
    executable: std::path::PathBuf,
    stealth: bool,
    allow_private_network: bool,
    proxy: Option<String>,
    user_agent: Option<String>,
    timeout_secs: u64,
    path: Option<std::ffi::OsString>,
    temp_dir: Option<std::path::PathBuf>,
}

impl SpawnSettings {
    fn from_config(cfg: &Config) -> Result<Self, String> {
        let bin = cfg.obscura_bin.as_deref().unwrap_or("obscura");
        let executable = resolve_trusted_program(bin).ok_or_else(|| {
            "obscura binary not found in a trusted directory (set obscura_bin / WEAVE_OBSCURA_BIN \
             to an installed `obscura`)"
                .to_string()
        })?;
        let proxy = cfg
            .obscura_proxy
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(proxy) = proxy.as_deref() {
            validate_proxy_arg(proxy)?;
        }
        let user_agent = cfg
            .obscura_user_agent
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let timeout_secs = cfg
            .obscura_timeout_secs
            .map(|n| (n.max(1) as u64).clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS))
            .unwrap_or(DEFAULT_OBSCURA_TIMEOUT_SECS);
        let temp_dir = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir());
        Ok(Self {
            executable,
            stealth: cfg.obscura_stealth.unwrap_or(false),
            allow_private_network: cfg.obscura_allow_internal.unwrap_or(false),
            proxy,
            user_agent,
            timeout_secs,
            path: std::env::var_os("PATH"),
            temp_dir,
        })
    }
}

struct CachedClient {
    settings: SpawnSettings,
    client: ObscuraClient,
}

/// The process-global cached obscura client. Like the injector, a single child is
/// spawned lazily and reused while its spawn settings remain identical; this avoids
/// threading a new field through the (already large) dispatch signature chain.
fn client_cell() -> &'static Mutex<Option<CachedClient>> {
    static CELL: OnceLock<Mutex<Option<CachedClient>>> = OnceLock::new();
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
    let wanted = SpawnSettings::from_config(cfg)?;
    let mut guard = client_cell()
        .lock()
        .map_err(|_| "obscura client lock poisoned".to_string())?;
    if guard
        .as_ref()
        .is_some_and(|cached| cached.settings != wanted)
    {
        // Drop kills and reaps the old child before a differently governed op can
        // reach it (notably true→false private-network policy changes).
        *guard = None;
    }
    if guard.is_none() {
        let client = ObscuraClient::spawn(&wanted)?;
        *guard = Some(CachedClient {
            settings: wanted,
            client,
        });
    }
    let client = &mut guard.as_mut().expect("just set").client;
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
#[derive(Debug)]
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
    requests: mpsc::Sender<WriteRequest>,
    replies: mpsc::Receiver<ReaderEvent>,
    next_id: u64,
    timeout: Duration,
}

impl ObscuraClient {
    /// Resolve, spawn, and handshake with `obscura mcp`. argv-only; trusted-path
    /// resolved; child stderr discarded (never logged — may carry target URLs).
    fn spawn(settings: &SpawnSettings) -> Result<ObscuraClient, String> {
        // Build the argv vector — NEVER a command string, never `sh -c`.
        let mut argv: Vec<String> = vec!["mcp".to_string()];
        if settings.stealth {
            argv.push("--stealth".to_string());
        }
        // Keep weave's direct-URL policy and obscura's redirect/subresource guard
        // aligned. Ambient OBSCURA_ALLOW_PRIVATE_NETWORK is scrubbed below; this
        // explicit upstream flag is the only way the governed child may opt in.
        if settings.allow_private_network {
            argv.push("--allow-private-network".to_string());
        }
        if let Some(proxy) = settings.proxy.as_deref() {
            argv.push("--proxy".to_string());
            argv.push(proxy.to_string());
        }
        if let Some(ua) = settings.user_agent.as_deref() {
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

        let timeout = Duration::from_secs(settings.timeout_secs);

        let mut command = Command::new(&settings.executable);
        command
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The child's stderr may carry target URLs or other provider detail — discard it,
            // NEVER pipe it into a weave log line (WL-048 redaction lesson).
            .stderr(Stdio::null());
        // Obscura has several ambient behaviour toggles, including one that disables
        // its private-network guard. Do not let inherited process state bypass weave's
        // policy or expose unrelated credentials to the browser child. PATH is
        // retained for runtime compatibility (the real binary resolves workers via
        // current_exe; test stubs use ordinary POSIX helpers). A validated absolute
        // TMPDIR is also retained because Nix sandboxes provide their only writable
        // temporary location that way and V8 needs temporary-file access.
        command.env_clear();
        if let Some(path) = settings.path.as_ref() {
            command.env("PATH", path);
        }
        if let Some(temp_dir) = settings.temp_dir.as_ref() {
            command.env("TMPDIR", temp_dir);
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
            requests: spawn_stdin_writer(stdin),
            replies: spawn_stdout_reader(stdout),
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

    /// Write a single capped newline-delimited JSON frame with a wall-clock bound.
    fn write_frame(&mut self, v: &Value) -> Result<(), WebErr> {
        let frame = encode_frame(v)?;
        let (acknowledgement, result) = mpsc::sync_channel(1);
        self.requests
            .send(WriteRequest {
                frame,
                acknowledgement,
            })
            .map_err(|_| WebErr::transport("obscura exited (write failed)"))?;
        match result.recv_timeout(self.timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => {
                self.terminate_child();
                Err(WebErr::transport(message))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.terminate_child();
                Err(WebErr::transport("obscura timed out while writing"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.terminate_child();
                Err(WebErr::transport("obscura exited (write failed)"))
            }
        }
    }

    /// Read newline-delimited reply lines, skipping notifications/other-id frames,
    /// until the line whose `id` matches `want`. Bounded by [`Self::timeout`] and
    /// [`MAX_LINE_BYTES`]. On timeout the child is killed+reaped and a transport
    /// error returned (the caller drops the cached client).
    fn read_reply_for(&mut self, want: u64) -> Result<Value, WebErr> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.terminate_child();
                return Err(WebErr::transport("obscura timed out"));
            }
            let line = match self.replies.recv_timeout(remaining) {
                Ok(ReaderEvent::Line(line)) => line,
                Ok(ReaderEvent::Eof) => {
                    self.terminate_child();
                    return Err(WebErr::transport("obscura exited (EOF)"));
                }
                Ok(ReaderEvent::Error(message)) => {
                    self.terminate_child();
                    return Err(WebErr::transport(message));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.terminate_child();
                    return Err(WebErr::transport("obscura timed out"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.terminate_child();
                    return Err(WebErr::transport("obscura exited (read failed)"));
                }
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

    fn terminate_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Blocking byte-capped line reader used only by the dedicated stdout thread.
/// Deadlines are enforced independently by `recv_timeout` on the request thread.
fn read_capped_line_from<R: Read>(reader: &mut R, cap: usize) -> Result<Option<String>, WebErr> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
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
                if buf.len() > cap {
                    return Err(WebErr::transport("obscura reply exceeded the line cap"));
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(WebErr::transport("obscura exited (read failed)")),
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

    // ---- production bounded-read / line-cap (the real code path, not a mirror) ----

    #[test]
    fn bounded_line_reads_a_normal_line() {
        // Drives the production reader-thread line parser against canned bytes.
        let mut cur = std::io::Cursor::new(b"hello world\n".to_vec());
        let line = read_capped_line_from(&mut cur, 1024).unwrap().unwrap();
        assert_eq!(line, "hello world");
    }

    #[test]
    fn bounded_line_eof_with_no_bytes_is_none() {
        let mut cur = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_capped_line_from(&mut cur, 1024).unwrap().is_none());
    }

    #[test]
    fn bounded_line_eof_with_unterminated_bytes_returns_buffer() {
        // A final line without a trailing newline is still returned (then the next
        // read yields None) — matches the production EOF branch.
        let mut cur = std::io::Cursor::new(b"tail-no-newline".to_vec());
        let line = read_capped_line_from(&mut cur, 1024).unwrap().unwrap();
        assert_eq!(line, "tail-no-newline");
    }

    #[test]
    fn oversized_line_exceeds_cap_is_transport_error() {
        // A response larger than the cap (no newline in sight) must be a clean
        // transport error — never an OOM, panic, or hang. Uses a tiny cap so the
        // test stays fast; the production cap is MAX_LINE_BYTES.
        let big = vec![b'a'; 4096]; // no '\n' ⇒ keeps growing past the cap
        let mut cur = std::io::Cursor::new(big);
        let err = read_capped_line_from(&mut cur, 64).unwrap_err();
        assert!(err.transport_fault, "cap breach must be a transport fault");
        assert!(
            err.message.contains("line cap"),
            "expected line-cap error, got: {}",
            err.message
        );
    }

    #[test]
    fn silent_live_child_obeys_reply_timeout() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn silent child");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut client = ObscuraClient {
            child,
            requests: spawn_stdin_writer(stdin),
            replies: spawn_stdout_reader(stdout),
            next_id: 0,
            timeout: Duration::from_millis(100),
        };
        let started = Instant::now();
        let err = client.read_reply_for(1).unwrap_err();
        assert!(err.transport_fault);
        assert!(err.message.contains("timed out"), "got: {}", err.message);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "silent child bypassed the configured deadline"
        );
    }

    #[test]
    fn non_reading_child_obeys_write_timeout() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn non-reading child");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut client = ObscuraClient {
            child,
            requests: spawn_stdin_writer(stdin),
            replies: spawn_stdout_reader(stdout),
            next_id: 0,
            timeout: Duration::from_millis(100),
        };
        let payload = "x".repeat(weave_core::store::MAX_BODY * 4);
        let started = Instant::now();
        let err = client
            .write_frame(&json!({"payload": payload}))
            .unwrap_err();
        assert!(err.transport_fault);
        assert!(
            err.message.contains("timed out while writing"),
            "got: {}",
            err.message
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "non-reading child bypassed the configured write deadline"
        );
    }

    #[test]
    fn oversized_outgoing_frame_is_rejected_before_write() {
        let payload = "x".repeat(MAX_FRAME_BYTES);
        let error = encode_frame(&json!({"payload": payload})).unwrap_err();
        assert!(error.transport_fault);
        assert!(error.message.contains("frame cap"));
    }

    #[test]
    fn proxy_arguments_reject_embedded_credentials_and_bad_schemes() {
        assert!(validate_proxy_arg("http://proxy.example:8080").is_ok());
        assert!(validate_proxy_arg("socks5://proxy.example:1080").is_ok());
        let credential_error =
            validate_proxy_arg("http://operator:secret@proxy.example:8080").unwrap_err();
        assert!(credential_error.contains("origin-only credential-free URL"));
        assert!(!credential_error.contains("operator"));
        assert!(!credential_error.contains("secret"));
        assert!(validate_proxy_arg("https://proxy.example/tenant-token").is_err());
        assert!(validate_proxy_arg("https://proxy.example/?token=secret").is_err());
        assert!(validate_proxy_arg("socks5://proxy.example:1080/#credential").is_err());
        assert!(validate_proxy_arg("file:///tmp/proxy.sock").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cached_child_respawns_when_spawn_settings_change() {
        use std::os::unix::fs::PermissionsExt;

        struct StopOnDrop;
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                stop();
            }
        }

        let _env = weave_core::testenv::lock_env();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "weave-obscura-respawn-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create fake obscura dir");
        let script = dir.join("obscura");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"serverInfo":{"name":"obscura-mcp"}}}\n' "$id" ;;
    *'notifications/initialized'*) : ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pid=%s args=%s"}]}}\n' "$id" "$$" "$*" ;;
  esac
done
"#,
        )
        .expect("write fake obscura");
        let mut permissions = std::fs::metadata(&script)
            .expect("stat fake obscura")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod fake obscura");

        let _mux = weave_core::testenv::EnvVarGuard::set(
            "WEAVE_MUX_DIR",
            dir.to_str().expect("utf-8 temp path"),
        );
        stop();
        let _stop = StopOnDrop;
        let mut cfg = Config {
            obscura_bin: Some("obscura".to_string()),
            ..Config::default()
        };
        let args = json!({});
        let first = call(&cfg, "browser_tab_list", &args).expect("first fake call");
        assert!(!first.contains("--allow-private-network"), "{first}");

        cfg.obscura_allow_internal = Some(true);
        let second = call(&cfg, "browser_tab_list", &args).expect("policy-change call");
        assert!(second.contains("--allow-private-network"), "{second}");
        let first_pid = first.split_whitespace().next().expect("first pid");
        let second_pid = second.split_whitespace().next().expect("second pid");
        assert_ne!(first_pid, second_pid, "policy change reused stale child");

        // Per-call allow-list changes do not affect the child and should not churn it.
        cfg.obscura_allow_ops = Some(vec!["tab_list".to_string()]);
        let third = call(&cfg, "browser_tab_list", &args).expect("policy-only call");
        assert_eq!(
            second_pid,
            third.split_whitespace().next().expect("third pid"),
            "per-call policy change unnecessarily respawned child"
        );

        // The read/write deadline is held by the client, so it is spawn-affecting too.
        cfg.obscura_timeout_secs = Some(31);
        let fourth = call(&cfg, "browser_tab_list", &args).expect("timeout-change call");
        let fourth_pid = fourth.split_whitespace().next().expect("fourth pid");
        assert_ne!(second_pid, fourth_pid, "timeout change reused stale client");

        // Revocation is the security-critical direction: a child started with
        // private-network access must never survive a true→false policy change.
        cfg.obscura_allow_internal = Some(false);
        let fifth = call(&cfg, "browser_tab_list", &args).expect("revocation call");
        assert!(!fifth.contains("--allow-private-network"), "{fifth}");
        assert_ne!(
            fourth_pid,
            fifth.split_whitespace().next().expect("fifth pid"),
            "private-network revocation reused permissive child"
        );

        stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn web_err_op_vs_transport_classification() {
        // An obscura tool error (child healthy) is NOT a transport fault; a wedged
        // pipe IS — the `call` wrapper relies on this to decide whether to re-spawn.
        assert!(!WebErr::op("Missing url").transport_fault);
        assert!(WebErr::transport("obscura exited").transport_fault);
    }
}
