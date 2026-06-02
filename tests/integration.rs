//! End-to-end tests that drive the built `weave` binary as a black box.
//!
//! Everything goes through `std::process::Command` (binary path resolved at
//! compile time via `CARGO_BIN_EXE_weave`). Each test uses its own unique temp
//! `WEAVE_DB` so they are isolated, run in parallel safely, and never touch the
//! real store. JSON is parsed with `serde_json` (already a dependency).
//!
//! Coverage:
//!   1. MCP stdio: initialize / tools/list / tools/call (send, inbox, read
//!      tracking), then close stdin to end the server.
//!   2. CLI roundtrip: send -> inbox, register -> peers.
//!   3. Native injector via a fake `tmux` on PATH that logs its argv.

mod common;

use common::{run, run_hook, run_ok, McpServer, TestDb};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

// ---------------------------------------------------------------------------
// 1. MCP stdio protocol
// ---------------------------------------------------------------------------

#[test]
fn mcp_stdio_initialize_list_and_send_inbox_roundtrip() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // initialize -> serverInfo.name == "weave"
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "weave-it", "version": "0"}
        }),
    );
    assert_eq!(
        init.pointer("/serverInfo/name").and_then(|v| v.as_str()),
        Some("weave"),
        "initialize serverInfo.name should be 'weave': {init}"
    );

    // The `notifications/initialized` notification must get NO reply.
    mcp.send_raw(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }));

    // tools/list -> contains weave_send, weave_inbox, weave_peers
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("tools/list should return a `tools` array")
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    for expected in ["weave_send", "weave_inbox", "weave_peers"] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing {expected}; got {names:?}"
        );
    }

    // tools/call weave_send {from, to, body} -> isError:false
    let (is_err, send_text) = mcp.call_tool(
        "weave_send",
        serde_json::json!({"from": "desktop", "to": "envctl", "body": "hi"}),
    );
    assert!(!is_err, "weave_send should not be an error: {send_text}");

    // tools/call weave_inbox {me: envctl} -> text contains "hi"
    let (is_err, inbox_text) = mcp.call_tool("weave_inbox", serde_json::json!({"me": "envctl"}));
    assert!(!is_err, "weave_inbox should not be an error: {inbox_text}");
    assert!(
        inbox_text.contains("hi"),
        "first weave_inbox should contain the body 'hi': {inbox_text:?}"
    );

    // second weave_inbox {me: envctl} -> no unread (read tracking worked)
    let (is_err, inbox_text2) = mcp.call_tool("weave_inbox", serde_json::json!({"me": "envctl"}));
    assert!(
        !is_err,
        "second weave_inbox should not be an error: {inbox_text2}"
    );
    assert!(
        inbox_text2.to_lowercase().contains("no unread"),
        "second weave_inbox should report no unread messages: {inbox_text2:?}"
    );

    // Closing stdin ends the server cleanly (and reaps the child).
    mcp.shutdown();
}

#[test]
fn mcp_unknown_method_returns_jsonrpc_error() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // A request with an id but an unsupported method must come back as an error
    // object (not a panic / not a silent drop).
    let id = 99;
    mcp.send_raw(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "does/not/exist",
        "params": {}
    }));
    let resp = mcp.recv_line();
    assert_eq!(resp.get("id").and_then(|v| v.as_i64()), Some(id));
    assert!(
        resp.get("error").is_some(),
        "unknown method should return a JSON-RPC error: {resp}"
    );

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// 2. CLI roundtrip
// ---------------------------------------------------------------------------

#[test]
fn cli_send_then_inbox_shows_body() {
    let db = TestDb::new();

    let sent = run_ok(
        &db,
        &["send", "--from", "a", "--to", "b", "--body", "hello"],
    );
    assert!(
        sent.contains("a -> b"),
        "send should confirm the route a -> b: {sent:?}"
    );

    // First read: the message is there.
    let inbox = run_ok(&db, &["inbox", "--me", "b"]);
    assert!(
        inbox.contains("hello"),
        "inbox for 'b' should contain 'hello': {inbox:?}"
    );
    assert!(
        inbox.contains("a -> b"),
        "inbox should show sender/recipient line: {inbox:?}"
    );

    // Default read marks messages read, so a second plain read is empty.
    let inbox2 = run_ok(&db, &["inbox", "--me", "b"]);
    assert!(
        inbox2.contains("empty"),
        "second inbox read for 'b' should be empty (read-tracked): {inbox2:?}"
    );

    // A different recipient never saw the message.
    let other = run_ok(&db, &["inbox", "--me", "c"]);
    assert!(
        other.contains("empty"),
        "inbox for unrelated 'c' should be empty: {other:?}"
    );
}

#[test]
fn cli_register_then_peers_lists_peer() {
    let db = TestDb::new();

    let reg = run_ok(&db, &["register", "--name", "z"]);
    assert!(
        reg.contains("registered 'z'"),
        "register should confirm peer 'z': {reg:?}"
    );

    let peers = run_ok(&db, &["peers"]);
    assert!(
        peers.contains('z'),
        "peers list should mention 'z': {peers:?}"
    );
    // 'z' was registered with no mux env present, so it is a non-injectable peer.
    assert!(
        peers.contains("no-inject"),
        "a peer registered outside any mux should be no-inject: {peers:?}"
    );
}

#[test]
fn cli_sessions_reports_activity_after_send() {
    let db = TestDb::new();

    // No traffic yet.
    let empty = run_ok(&db, &["sessions"]);
    assert!(
        empty.contains("no sessions yet"),
        "fresh store should report no sessions: {empty:?}"
    );

    run_ok(&db, &["send", "--from", "a", "--to", "b", "--body", "ping"]);

    // 'b' now has one unread message.
    let sessions = run_ok(&db, &["sessions"]);
    assert!(
        sessions.contains('b') && sessions.contains("unread"),
        "sessions should report unread counts after a send: {sessions:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Native injector via a fake mux
// ---------------------------------------------------------------------------

/// Create a temp dir containing an executable `tmux` shell script that appends
/// its full argv (one invocation per line) to `log_path`. Returns the dir.
fn make_fake_tmux(log_path: &Path) -> std::path::PathBuf {
    let dir = common::unique_db().with_extension("muxbin");
    std::fs::create_dir_all(&dir).expect("create fake-mux bin dir");

    let script = dir.join("tmux");
    // `"$@"` captures every arg verbatim; we record the joined argv plus a
    // marker so the test can grep for `send-keys` regardless of arg spacing.
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
        log_path.display()
    );
    std::fs::write(&script, body).expect("write fake tmux script");
    let mut perms = std::fs::metadata(&script)
        .expect("stat fake tmux")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod +x fake tmux");

    dir
}

/// Build a `weave` command with the fake-mux dir prepended to PATH so the
/// injector resolves our script instead of any real tmux.
fn weave_with_fake_path(
    db: &TestDb,
    fake_dir: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Command {
    let mut cmd = common::weave_cmd(db, args);
    let orig = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fake_dir.display(), orig);
    cmd.env("PATH", new_path);
    // The injector only runs a mux from a TRUSTED dir (never ambient $PATH); the
    // fake-mux dir is trusted explicitly via WEAVE_MUX_DIR (the test opt-in).
    cmd.env("WEAVE_MUX_DIR", fake_dir);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd
}

#[test]
fn injector_send_drives_fake_tmux() {
    let db = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    // Register peer 'p' while pretending to live in tmux pane %1, with the fake
    // tmux on PATH. Result: peer p => mux=tmux, target=%1 (injectable).
    let reg_status = weave_with_fake_path(
        &db,
        &fake_dir,
        &[("TMUX_PANE", "%1")],
        &["register", "--name", "p"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg_status.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg_status.stderr)
    );

    // Confirm the peer is injectable on tmux (no TMUX_PANE needed for a plain
    // listing; fake tmux just needs to exist so `have()` sees it).
    let peers_out = weave_with_fake_path(&db, &fake_dir, &[], &["peers"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn peers");
    let peers_txt = String::from_utf8_lossy(&peers_out.stdout);
    assert!(
        peers_txt.contains("[tmux]") && peers_txt.contains("injectable"),
        "peer 'p' should be an injectable tmux peer: {peers_txt:?}"
    );

    // Send to 'p'. The injector should drive the fake tmux with `send-keys`.
    let send_out = weave_with_fake_path(
        &db,
        &fake_dir,
        &[],
        &["send", "--from", "desktop", "--to", "p", "--body", "x"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn send");
    assert!(
        send_out.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&send_out.stderr)
    );

    let logged = read_log_with_retries(&log);
    assert!(
        logged.contains("send-keys"),
        "fake tmux log should show a send-keys invocation after send; got:\n{logged}"
    );
    // The literal body must have been typed into pane %1.
    assert!(
        logged.contains("-t %1") && logged.contains(" x"),
        "fake tmux log should target pane %1 and type the body 'x':\n{logged}"
    );
}

#[test]
fn injector_explicit_inject_drives_fake_tmux() {
    // Fallback path the task allows: `weave inject --to p --text hi` must invoke
    // the fake tmux even without a send.
    let db = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    let reg = weave_with_fake_path(
        &db,
        &fake_dir,
        &[("TMUX_PANE", "%7")],
        &["register", "--name", "p"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(reg.status.success(), "register failed");

    let inj = weave_with_fake_path(
        &db,
        &fake_dir,
        &[],
        &["inject", "--to", "p", "--text", "hi"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn inject");
    assert!(
        inj.status.success(),
        "inject failed: {}",
        String::from_utf8_lossy(&inj.stderr)
    );
    let out_txt = String::from_utf8_lossy(&inj.stdout);
    assert!(
        out_txt.contains("injected"),
        "inject should report success: {out_txt:?}"
    );

    let logged = read_log_with_retries(&log);
    assert!(
        logged.contains("send-keys") && logged.contains("-t %7"),
        "fake tmux log should show send-keys to pane %7:\n{logged}"
    );
    assert!(
        logged.contains(" hi"),
        "fake tmux log should type the injected text 'hi':\n{logged}"
    );
}

/// The fake-mux script writes asynchronously relative to our process; read the
/// log a few times with a short backoff so we never flake and never hang.
fn read_log_with_retries(log: &Path) -> String {
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(log) {
            if !s.is_empty() {
                return s;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::read_to_string(log).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Sanity: the binary path resolved and a bad invocation fails cleanly.
// ---------------------------------------------------------------------------

#[test]
fn binary_rejects_unknown_subcommand() {
    let db = TestDb::new();
    let (ok, _out, err) = run(&db, &["definitely-not-a-subcommand"]);
    assert!(!ok, "an unknown subcommand should exit non-zero");
    assert!(
        !err.is_empty(),
        "clap should print a usage/error message on stderr"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle hooks (weave hook session|prompt|stop) — the Claude Code integration.
// ---------------------------------------------------------------------------

#[test]
fn hook_session_registers_and_prompt_drains_marks_read() {
    let db = TestDb::new();
    // SessionStart registers a peer named from the payload cwd basename.
    let (ok, _o, _e) = run_hook(&db, "session", r#"{"cwd":"/proj/alpha"}"#);
    assert!(ok);
    assert!(run_ok(&db, &["peers"]).contains("alpha"), "peer registered");

    run_ok(
        &db,
        &[
            "send",
            "--from",
            "bob",
            "--to",
            "alpha",
            "--body",
            "hello-hook",
        ],
    );

    // UserPromptSubmit drains to stdout AND (with an explicit payload cwd) marks read.
    let (ok, out, _e) = run_hook(&db, "prompt", r#"{"cwd":"/proj/alpha"}"#);
    assert!(ok);
    assert!(
        out.contains("new message(s) for 'alpha'"),
        "drain header: {out}"
    );
    assert!(
        out.contains("from bob") && out.contains("hello-hook"),
        "body surfaced: {out}"
    );

    // Second prompt: already drained+marked, nothing left.
    let (_ok, out2, _e) = run_hook(&db, "prompt", r#"{"cwd":"/proj/alpha"}"#);
    assert!(
        !out2.contains("hello-hook"),
        "message must be marked read: {out2}"
    );
}

#[test]
fn hook_stop_peeks_and_does_not_consume() {
    let db = TestDb::new();
    run_hook(&db, "session", r#"{"cwd":"/proj/beta"}"#);
    run_ok(
        &db,
        &[
            "send",
            "--from",
            "x",
            "--to",
            "beta",
            "--body",
            "stoppayload",
        ],
    );

    // Stop is a PEEK: two stops both re-surface the same message (never marked).
    let (_o1, out1, _e1) = run_hook(&db, "stop", r#"{"cwd":"/proj/beta"}"#);
    let (_o2, out2, _e2) = run_hook(&db, "stop", r#"{"cwd":"/proj/beta"}"#);
    assert!(out1.contains("stoppayload"), "stop #1 surfaces: {out1}");
    assert!(
        out2.contains("stoppayload"),
        "stop #2 still surfaces (not consumed): {out2}"
    );

    // prompt is the real drain.
    let (_o, outp, _e) = run_hook(&db, "prompt", r#"{"cwd":"/proj/beta"}"#);
    assert!(outp.contains("stoppayload"));
    let (_o, outp2, _e) = run_hook(&db, "prompt", r#"{"cwd":"/proj/beta"}"#);
    assert!(!outp2.contains("stoppayload"), "prompt consumed it");
}

#[test]
fn hook_guessed_identity_peeks_only_and_warns() {
    let db = TestDb::new();
    let dir = std::env::temp_dir()
        .join(format!("weave-guess-{}", std::process::id()))
        .join("proj");
    // Send to the dir basename "proj"; an empty-stdin hook guesses identity from cwd.
    run_ok(
        &db,
        &["send", "--from", "bob", "--to", "proj", "--body", "guessme"],
    );

    // Empty payload => not explicit => peek + warning; message is NOT consumed.
    let (_o1, out1, err1) = common::run_stdin_full(&db, &["hook", "prompt"], "", Some(&dir), &[]);
    assert!(
        err1.contains("no explicit session identity"),
        "warns: {err1}"
    );
    assert!(out1.contains("guessme"), "still surfaces: {out1}");
    let (_o2, out2, _e2) = common::run_stdin_full(&db, &["hook", "prompt"], "", Some(&dir), &[]);
    assert!(
        out2.contains("guessme"),
        "guessed peek must not consume: {out2}"
    );
}

#[test]
fn hook_tolerates_garbage_and_unknown_events() {
    let db = TestDb::new();
    let (ok, _o, err) = run_hook(&db, "prompt", "not json {{{");
    assert!(ok, "garbage payload must not fail the hook");
    assert!(err.contains("not valid JSON"), "warns on bad JSON: {err}");

    let (ok, out, _e) = run_hook(&db, "notification", "{}");
    assert!(ok && out.trim().is_empty(), "notification is a no-op");

    let (ok, _o, err) = run_hook(&db, "some-bogus-event", "{}");
    assert!(ok, "unknown event exits 0");
    assert!(
        err.contains("unknown hook event"),
        "warns on unknown event: {err}"
    );
}

// ---------------------------------------------------------------------------
// New CLI surface: --json output, doctor, gc, backend selection.
// ---------------------------------------------------------------------------

#[test]
fn read_commands_emit_valid_json() {
    let db = TestDb::new();
    run_ok(&db, &["send", "--from", "a", "--to", "b", "--body", "hi"]);
    run_ok(&db, &["register", "--name", "z"]);

    let inbox = run_ok(&db, &["inbox", "--me", "b", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox --json parses");
    assert_eq!(v["messages"][0]["body"], "hi");

    let peers = run_ok(&db, &["peers", "--json"]);
    let pv: serde_json::Value = serde_json::from_str(&peers).expect("peers --json parses");
    assert!(pv.as_array().unwrap().iter().any(|p| p["name"] == "z"));

    let sessions = run_ok(&db, &["sessions", "--json"]);
    let sv: serde_json::Value = serde_json::from_str(&sessions).expect("sessions --json parses");
    assert!(sv.is_array());
}

#[test]
fn doctor_json_reports_backend() {
    let db = TestDb::new();
    let out = run_ok(&db, &["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    // The backend reported is whichever was compiled in (sqlite by default, libsql
    // under --no-default-features --features libsql); both are valid.
    let backend = v["backend"].as_str().unwrap_or("");
    assert!(
        backend == "sqlite" || backend == "libsql",
        "doctor backend should be a known store, got {backend:?}"
    );
    assert!(v["db_path"].as_str().unwrap().contains("weave-it-"));
}

#[test]
fn gc_runs_and_reports() {
    let db = TestDb::new();
    run_ok(&db, &["send", "--from", "a", "--to", "b", "--body", "x"]);
    let out = run_ok(&db, &["gc", "--older-than-secs", "999999999"]);
    assert!(out.contains("gc: deleted"), "gc reports a count: {out}");
}

#[test]
fn unknown_backend_errors_loudly() {
    let db = TestDb::new();
    let (ok, _o, err) =
        common::run_stdin_full(&db, &["sessions"], "", None, &[("WEAVE_BACKEND", "bogus")]);
    assert!(!ok, "an unknown backend must fail, not silently default");
    assert!(err.contains("unknown backend"), "clear error: {err}");
}

#[test]
fn reply_thread_receipts_roundtrip() {
    let db = TestDb::new();
    run_ok(
        &db,
        &[
            "send",
            "--from",
            "a",
            "--to",
            "b",
            "--subject",
            "hi",
            "--body",
            "hello",
        ],
    );
    // Reply to #1 from b — auto-addressed back to the original sender (a).
    run_ok(
        &db,
        &[
            "reply",
            "--in-reply-to",
            "1",
            "--from",
            "b",
            "--body",
            "reply-body",
        ],
    );
    // Thread view shows both the root and the reply.
    let thr = run_ok(&db, &["thread", "--root", "1"]);
    assert!(
        thr.contains("hello") && thr.contains("reply-body"),
        "thread: {thr}"
    );
    // `a` drains its inbox (sees reply #2) -> a read receipt is recorded for #2.
    let ai = run_ok(&db, &["inbox", "--me", "a"]);
    assert!(
        ai.contains("reply-body"),
        "a should receive the reply: {ai}"
    );
    let rec = run_ok(&db, &["receipts", "--id", "2"]);
    assert!(
        rec.contains('a'),
        "receipts for #2 should list reader a: {rec}"
    );
}
