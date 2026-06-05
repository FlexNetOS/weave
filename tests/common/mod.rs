//! Shared helpers for weave end-to-end tests.
//!
//! Everything here drives the *built* `weave` binary as a black box via
//! `std::process::Command`. No new dependencies: `serde_json` is already a
//! crate dependency, and the MCP plumbing uses only `std`.
//!
//! Isolation: every test gets a unique temp `WEAVE_DB` file and we strip all
//! other `WEAVE_*` / mux env vars off the child so identity resolution and the
//! storage backend are fully deterministic and never touch the real store.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Hard cap on any single blocking read from a child so a hung/broken binary
/// fails the test instead of wedging the suite.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Absolute path to the freshly built `weave` binary (resolved at compile time).
pub fn weave_bin() -> &'static str {
    env!("CARGO_BIN_EXE_weave")
}

/// A unique temp DB path for one test. Combines the pid with a monotonic counter
/// so parallel tests never collide. The file does not need to pre-exist; the
/// sqlite backend creates it on open.
pub fn unique_db() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("weave-it-{pid}-{n}-{nanos}.db"))
}

/// A test-scoped temp DB that cleans up its sqlite files on drop.
pub struct TestDb {
    pub path: PathBuf,
}

impl TestDb {
    pub fn new() -> Self {
        TestDb { path: unique_db() }
    }

    /// The DB path as a string, for `.env("WEAVE_DB", ...)`.
    pub fn path_str(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Default for TestDb {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Best-effort cleanup of the db and any sqlite sidecar files.
        let base = self.path.to_string_lossy().into_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{base}{suffix}"));
        }
    }
}

/// Build a `weave <args...>` command pinned to `db`, with a clean environment:
/// inherited PATH (so the binary can find muxes if a test injects a fake one),
/// but every `WEAVE_*` and mux-detection var cleared so identity/backend are
/// deterministic.
pub fn weave_cmd(db: &TestDb, args: &[&str]) -> Command {
    let mut cmd = Command::new(weave_bin());
    cmd.args(args);
    scrub_env(&mut cmd);
    cmd.env("WEAVE_DB", db.path_str());
    cmd
}

/// Remove env vars that would otherwise make tests non-deterministic.
pub fn scrub_env(cmd: &mut Command) {
    for k in [
        "WEAVE_SESSION",
        "WEAVE_BACKEND",
        "WEAVE_DB",
        "WEAVE_LIBSQL_URL",
        "WEAVE_LIBSQL_AUTH_TOKEN",
        // Mux auto-detection vars — keep the harness's real terminal out of it.
        "TMUX_PANE",
        "ZELLIJ_SESSION_NAME",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "STY",
        // Force sqlite even if a stray config points elsewhere.
        "XDG_CONFIG_HOME",
    ] {
        cmd.env_remove(k);
    }
    // Point config discovery at an empty dir so no real config.toml is read.
    cmd.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("weave-it-noconfig"),
    );
}

/// Run a `weave` subcommand to completion and capture stdout/stderr as UTF-8.
/// Panics if the process cannot be spawned. Returns (success, stdout, stderr).
pub fn run(db: &TestDb, args: &[&str]) -> (bool, String, String) {
    let out = weave_cmd(db, args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn weave {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Like [`run`] but with extra environment variables applied AFTER [`scrub_env`]
/// (so they win — e.g. `WEAVE_PEER_DBS` for Tier-1 federation tests). Returns
/// (success, stdout, stderr).
pub fn run_env(db: &TestDb, args: &[&str], extra_env: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = weave_cmd(db, args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn weave {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Like [`run_ok`] but with extra environment variables (see [`run_env`]).
/// Asserts success and returns stdout.
pub fn run_ok_env(db: &TestDb, args: &[&str], extra_env: &[(&str, &str)]) -> String {
    let (ok, out, err) = run_env(db, args, extra_env);
    assert!(
        ok,
        "`weave {args:?}` (env {extra_env:?}) exited non-zero\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    out
}

/// Like [`run`] but asserts success and returns stdout. The full output is shown
/// on failure for easy debugging.
pub fn run_ok(db: &TestDb, args: &[&str]) -> String {
    let (ok, out, err) = run(db, args);
    assert!(
        ok,
        "`weave {args:?}` exited non-zero\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    out
}

/// Run a `weave` subcommand feeding `stdin` to it, optionally pinning the child's
/// working directory and adding extra env vars (applied AFTER scrub_env, so they
/// win). Returns (success, stdout, stderr). Used for the lifecycle-hook commands,
/// which read a JSON payload on stdin.
pub fn run_stdin_full(
    db: &TestDb,
    args: &[&str],
    stdin: &str,
    cwd: Option<&std::path::Path>,
    extra_env: &[(&str, &str)],
) -> (bool, String, String) {
    let mut cmd = weave_cmd(db, args);
    if let Some(d) = cwd {
        std::fs::create_dir_all(d).ok();
        cmd.current_dir(d);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn weave {args:?}: {e}"));
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin.as_bytes())
        .expect("write child stdin");
    let out = child.wait_with_output().expect("wait_with_output");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Convenience: feed a JSON hook payload to `weave hook <event>`.
pub fn run_hook(db: &TestDb, event: &str, payload: &str) -> (bool, String, String) {
    run_stdin_full(db, &["hook", event], payload, None, &[])
}

/// Run a `weave <args...>` subcommand with the child's working directory pinned to
/// `cwd` (no stdin), capturing output. This is the seam that exercises cwd-derived
/// git session tagging: point `cwd` at a temp dir containing a crafted `.git` file
/// so `weave register`/`scan` capture deterministic worktree tags without a real
/// repo or `git` binary. Returns (success, stdout, stderr).
pub fn run_in_cwd(db: &TestDb, args: &[&str], cwd: &std::path::Path) -> (bool, String, String) {
    run_stdin_full(db, args, "", Some(cwd), &[])
}

/// A live `weave mcp` server you talk to over newline-delimited JSON-RPC.
///
/// Reads happen on a background thread that pushes whole lines down a channel,
/// so [`recv_line`] can enforce a timeout and never block the test forever.
pub struct McpServer {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
}

impl McpServer {
    /// Spawn `weave mcp` against `db` and return a handle ready for requests.
    pub fn spawn(db: &TestDb) -> Self {
        Self::spawn_env(db, &[])
    }

    /// Spawn `weave mcp` against `db` with extra env applied after [`scrub_env`]
    /// (so they win — e.g. `WEAVE_PEER_DBS` for federation tests).
    pub fn spawn_env(db: &TestDb, extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_full(db, &["mcp"], extra_env, None)
    }

    /// Spawn `weave <args...>` (used for `["mcp", "--session", ...]`) against `db`
    /// with extra env and an optional working directory. Pinning the child's cwd is
    /// the seam that exercises `resolve_me`'s `basename(cwd)` fallback (the MCP
    /// server has no per-call `--from` flag), so identity-fallback tests can run the
    /// server in a temp dir whose basename is a known valid session name.
    pub fn spawn_full(
        db: &TestDb,
        args: &[&str],
        extra_env: &[(&str, &str)],
        cwd: Option<&std::path::Path>,
    ) -> Self {
        let mut cmd = weave_cmd(db, args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        if let Some(d) = cwd {
            std::fs::create_dir_all(d).ok();
            cmd.current_dir(d);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn `weave mcp`");

        let stdin = child.stdin.take().expect("mcp child has no stdin");
        let stdout = child.stdout.take().expect("mcp child has no stdout");

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        // Channel closed => test dropped the server; stop reading.
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        McpServer {
            child,
            stdin: Some(stdin),
            lines: rx,
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// Write one JSON value as a single newline-terminated line to the server.
    pub fn send_raw(&mut self, v: &serde_json::Value) {
        let mut line = serde_json::to_string(v).expect("serialize request");
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("mcp stdin already closed");
        stdin
            .write_all(line.as_bytes())
            .expect("write to mcp stdin");
        stdin.flush().expect("flush mcp stdin");
    }

    /// Receive the next response line, parsed as JSON, within the timeout.
    pub fn recv_line(&self) -> serde_json::Value {
        match self.lines.recv_timeout(READ_TIMEOUT) {
            Ok(l) => serde_json::from_str(&l)
                .unwrap_or_else(|e| panic!("mcp returned non-JSON line {l:?}: {e}")),
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for an mcp response (>{READ_TIMEOUT:?})")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("mcp server closed stdout before responding")
            }
        }
    }

    /// Send a JSON-RPC request (auto-assigned id) and return its parsed result
    /// object (the value of the top-level `result` field). Asserts no error and
    /// that the id matches.
    pub fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.alloc_id();
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let resp = self.recv_line();
        assert_eq!(
            resp.get("id").and_then(|v| v.as_i64()),
            Some(id),
            "response id mismatch for {method}: {resp}"
        );
        assert!(
            resp.get("error").is_none(),
            "{method} returned a JSON-RPC error: {resp}"
        );
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} response had no `result`: {resp}"))
    }

    /// Convenience: call a tool and return (is_error, joined_text).
    pub fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> (bool, String) {
        let result = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        );
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        (is_error, text)
    }

    /// Gracefully shut down: drop stdin so the server's read loop hits EOF and
    /// exits on its own (exercising the real "stdin closed; exiting" path), then
    /// reap it. Falls back to kill if it doesn't exit within the timeout.
    pub fn shutdown(mut self) {
        // Close stdin -> EOF -> server exits its read loop.
        drop(self.stdin.take());

        // Poll for clean exit, then kill if needed so we never hang.
        let deadline = std::time::Instant::now() + READ_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => return, // exited cleanly on EOF
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Ensure no orphaned child if a test panics mid-flight or skips shutdown.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
