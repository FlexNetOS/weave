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

use common::{run, run_env, run_hook, run_ok, run_ok_env, McpServer, TestDb};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

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
    // The liveness probe must have been served by the FAKE tmux (not a real
    // /usr/bin/tmux): the fake records every argv, so a `has-session` line proves
    // WEAVE_MUX_DIR took precedence over the system dir on this runner.
    assert!(
        logged.contains("has-session"),
        "fake tmux log should record the has-session liveness probe:\n{logged}"
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
    // The liveness probe must have been served by the FAKE tmux (not a real
    // /usr/bin/tmux): a recorded `has-session` line proves WEAVE_MUX_DIR took
    // precedence over the system dir on this runner.
    assert!(
        logged.contains("has-session"),
        "fake tmux log should record the has-session liveness probe:\n{logged}"
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

// ---------------------------------------------------------------------------
// Presence & Live-Connect (Phase 1): attach (B1), connect (C2), heartbeat (A1),
// doctor non-default-DB hint (FR6).
// ---------------------------------------------------------------------------

/// B1 zero-restart adoption: a peer first registered with NO mux env (so it is
/// `no-inject`) becomes `injectable` after `weave attach` is run inside a (fake)
/// tmux — without restarting under the SessionStart hook. Proven via the
/// `peers --json` `injectable` field flipping false -> true.
#[test]
fn attach_flips_no_inject_peer_to_injectable_under_fake_mux() {
    let db = TestDb::new();

    // 1. Register 'p' with no mux env present -> a non-injectable (mux=none) peer.
    run_ok(&db, &["register", "--name", "p"]);
    let before = run_ok(&db, &["peers", "--json"]);
    let bv: serde_json::Value = serde_json::from_str(&before).expect("peers --json parses");
    let p_before = bv
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "p")
        .expect("peer p listed before attach");
    assert_eq!(
        p_before["injectable"],
        serde_json::Value::Bool(false),
        "peer 'p' starts no-inject (mux=none): {p_before}"
    );

    // 2. Run `weave attach --name p` inside a fake tmux pane %9. This re-captures
    //    the live pane env and upserts ONLY p's own row.
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);
    let attach = weave_with_fake_path(
        &db,
        &fake_dir,
        &[("TMUX_PANE", "%9")],
        &["attach", "--name", "p"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn attach");
    assert!(
        attach.status.success(),
        "attach failed: {}",
        String::from_utf8_lossy(&attach.stderr)
    );
    let attach_out = String::from_utf8_lossy(&attach.stdout);
    assert!(
        attach_out.contains("attached 'p'")
            && attach_out.contains("[tmux]")
            && attach_out.contains("injectable"),
        "attach should report p now injectable on tmux: {attach_out:?}"
    );

    // 3. peers --json now reports p injectable on tmux, target %9 — adopted with
    //    zero restart.
    let after = run_ok(&db, &["peers", "--json"]);
    let av: serde_json::Value = serde_json::from_str(&after).expect("peers --json parses");
    let p_after = av
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "p")
        .expect("peer p listed after attach");
    assert_eq!(
        p_after["injectable"],
        serde_json::Value::Bool(true),
        "peer 'p' flipped to injectable after attach: {p_after}"
    );
    assert_eq!(p_after["mux"], "tmux", "mux re-captured: {p_after}");
    assert_eq!(p_after["target"], "%9", "pane id re-captured: {p_after}");
}

/// C2 connect verdict strings under a fake mux:
/// - an injectable, fake-tmux-alive peer reports "live";
/// - a `mux=none` peer reports "not injectable" + the will-queue message;
/// - a non-existent peer exits non-zero (CLI error).
#[test]
fn connect_cli_verdict_strings() {
    let db = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    // Injectable peer 'live1' on fake tmux pane %1; the fake `has-session` exits 0
    // so target_alive is true -> Live.
    let reg = weave_with_fake_path(
        &db,
        &fake_dir,
        &[("TMUX_PANE", "%1")],
        &["register", "--name", "live1"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register live1");
    assert!(reg.status.success(), "register live1 failed");

    let conn = weave_with_fake_path(&db, &fake_dir, &[], &["connect", "--to", "live1"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn connect live1");
    assert!(
        conn.status.success(),
        "connect to a live peer must not be an error: {}",
        String::from_utf8_lossy(&conn.stderr)
    );
    let conn_out = String::from_utf8_lossy(&conn.stdout);
    assert!(
        conn_out.contains("connect 'live1': live"),
        "connect to injectable+alive peer reports live: {conn_out:?}"
    );

    // Non-injectable peer 'queued' (registered with no mux) -> not injectable, will
    // queue, and this is NOT an error (exit 0).
    run_ok(&db, &["register", "--name", "queued"]);
    let (ok, out, _err) = run(&db, &["connect", "--to", "queued"]);
    assert!(
        ok,
        "connect to a non-injectable peer is graceful, not an error"
    );
    assert!(
        out.contains("connect 'queued': not injectable (mux=none)")
            && out.contains("delivery will be queued"),
        "connect to mux=none peer reports queue fallback: {out:?}"
    );

    // Non-existent peer -> hard error (exit non-zero).
    let (ok, _out, err) = run(&db, &["connect", "--to", "ghost"]);
    assert!(!ok, "connect to a non-existent peer must be an error");
    assert!(
        err.contains("no registered peer 'ghost'"),
        "clear not-found error: {err:?}"
    );
}

/// A1 heartbeat-on-read: running `weave peers` with an EXPLICIT identity
/// (`WEAVE_SESSION`) touches the caller's own `last_seen`. We assert the
/// heartbeat never regresses `last_seen` (it is `>=` the value at registration).
/// (last_seen has 1s granularity, so we assert non-regression rather than a
/// strict increase to stay non-flaky.)
///
/// NOTE (A2): `register` runs in a short-lived subprocess, so the PID it persists
/// is dead by the time we read back. With A2 real-liveness, a dead-PID peer on the
/// local host reads `online:false` regardless of recency — so this test asserts
/// the A1 heartbeat property (`last_seen` non-regression) only. The dead-PID
/// offline behavior is asserted directly below.
#[test]
fn peers_read_heartbeats_explicit_identity() {
    let db = TestDb::new();
    // Register 'hb' as our own peer (sets last_seen = now).
    run_ok(&db, &["register", "--name", "hb"]);

    let read_last_seen = |db: &TestDb| -> i64 {
        let j = run_ok(db, &["peers", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&j).expect("peers --json parses");
        v.as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "hb")
            .and_then(|x| x["last_seen"].as_i64())
            .expect("hb last_seen present")
    };

    let t0 = read_last_seen(&db);

    // Read `peers` with an explicit identity -> refresh_presence touches 'hb'.
    let (ok, _o, _e) =
        common::run_stdin_full(&db, &["peers"], "", None, &[("WEAVE_SESSION", "hb")]);
    assert!(ok, "peers with explicit identity must succeed");

    let t1 = read_last_seen(&db);
    assert!(
        t1 >= t0,
        "heartbeat must never regress last_seen (was {t0}, now {t1})"
    );
}

/// A2 real-liveness: a peer whose registering process has exited reads
/// `online:false` (and `alive:false`) on the local host even though its
/// `last_seen` is within the TTL window — recency alone is no longer enough. The
/// `register` subprocess that wrote the row is dead by the time we read it back,
/// so the persisted local PID fails the `/proc/<pid>` liveness probe.
#[test]
fn peers_dead_local_pid_reads_offline_despite_recency() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "gone"]);

    let j = run_ok(&db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&j).expect("peers --json parses");
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "gone")
        .expect("gone peer present");

    // The row carries a (now-dead) pid and a non-empty host.
    assert!(row["pid"].as_i64().is_some(), "pid was persisted");
    assert!(
        !row["host"].as_str().unwrap_or("").is_empty(),
        "host was persisted"
    );

    // On Linux the dead PID is probed and the peer reads offline. On non-Linux the
    // probe degrades to TTL-only, so we only assert the offline behavior where the
    // probe is real.
    if cfg!(target_os = "linux") {
        assert_eq!(
            row["online"].as_bool(),
            Some(false),
            "dead local PID must read offline under A2 liveness"
        );
        assert_eq!(row["alive"].as_bool(), Some(false), "alive mirrors online");
    }
}

/// A2 `peers --json` shape: a peer registered by a STILL-LIVE process exposes
/// `pid` (an integer), `host` (a non-empty string), and `alive:true`. We drive
/// the registration through a long-lived `weave mcp` server (`weave_attach`
/// captures the server's own pid/host) and read it back via the black-box CLI
/// while the server is still running, so on Linux the persisted pid passes the
/// `/proc` liveness probe and the peer reads `alive:true`. This proves the JSON
/// types and that a genuinely-live session is not misreported offline.
#[test]
fn peers_json_live_session_reports_pid_host_and_alive_true() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // The live MCP server attaches itself (capturing its OWN live pid + host).
    let (err, text) = mcp.call_tool("weave_attach", serde_json::json!({"me": "liveone"}));
    assert!(!err, "attach must succeed: {text}");

    // Read the peer back via the CLI while the server is still alive.
    let j = run_ok(&db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&j).expect("peers --json parses");
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "liveone")
        .expect("liveone peer present");

    // Types: pid is an integer, host a non-empty string, alive a bool, and the
    // legacy `online` field mirrors `alive`.
    assert!(row["pid"].as_i64().is_some(), "pid is an integer: {row}");
    assert!(
        !row["host"].as_str().unwrap_or("").is_empty(),
        "host is a non-empty string: {row}"
    );
    assert!(row["alive"].is_boolean(), "alive is a bool: {row}");
    assert_eq!(
        row["online"], row["alive"],
        "online mirrors the resolved alive verdict"
    );

    // On Linux the live server's pid is probed and the peer reads alive:true.
    if cfg!(target_os = "linux") {
        assert_eq!(
            row["alive"].as_bool(),
            Some(true),
            "a still-live session must read alive:true: {row}"
        );
    }

    mcp.shutdown();
}

/// FR6: `doctor --json` carries `db_is_default`. Under our test (non-default)
/// WEAVE_DB it is `false` and the text form prints the hint; under the default
/// store path it is `true`.
#[test]
fn doctor_json_reports_db_is_default() {
    // Non-default WEAVE_DB (every TestDb is a unique temp path) -> false + hint.
    let db = TestDb::new();
    let out = run_ok(&db, &["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["db_is_default"],
        serde_json::Value::Bool(false),
        "a temp WEAVE_DB is not the XDG default: {v}"
    );
    let txt = run_ok(&db, &["doctor"]);
    assert!(
        txt.contains("non-default WEAVE_DB"),
        "text doctor prints the non-default hint: {txt:?}"
    );

    // Default store path: clear WEAVE_DB and point XDG_DATA_HOME at a temp dir so
    // the resolved default path equals config::default_db_path() in this process.
    let xdg = std::env::temp_dir().join(format!(
        "weave-it-xdgdata-{}-{}",
        std::process::id(),
        common::unique_db().file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&xdg).ok();
    let mut cmd = Command::new(common::weave_bin());
    cmd.args(["doctor", "--json"]);
    common::scrub_env(&mut cmd);
    cmd.env_remove("WEAVE_DB");
    cmd.env("XDG_DATA_HOME", &xdg);
    let default_out = cmd
        .stdin(Stdio::null())
        .output()
        .expect("spawn doctor (default db)");
    assert!(
        default_out.status.success(),
        "doctor (default db) failed: {}",
        String::from_utf8_lossy(&default_out.stderr)
    );
    let dv: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&default_out.stdout))
        .expect("doctor --json parses");
    assert_eq!(
        dv["db_is_default"],
        serde_json::Value::Bool(true),
        "resolved default WEAVE_DB should report db_is_default=true: {dv}"
    );
}

// ---------------------------------------------------------------------------
// MCP: weave_attach / weave_connect (incl. failure paths).
// ---------------------------------------------------------------------------

/// `weave_attach` upserts the caller's own peer, which `weave_peers` then lists.
/// Failure path: an empty `me` (no server default) and an oversized `me` both
/// return `isError`, never a panic or a silent persist.
#[test]
fn mcp_attach_upserts_and_rejects_bad_identity() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // Success: explicit identity 'agentA' is upserted (the MCP server has no mux
    // env, so it lands as a no-inject peer — but it IS now visible).
    let (err, text) = mcp.call_tool("weave_attach", serde_json::json!({"me": "agentA"}));
    assert!(!err, "attach with a valid identity must succeed: {text}");
    assert!(
        text.contains("Attached 'agentA'"),
        "attach reports success: {text:?}"
    );

    // weave_peers now lists agentA.
    let (perr, ptext) = mcp.call_tool("weave_peers", serde_json::json!({}));
    assert!(!perr, "weave_peers must succeed: {ptext}");
    assert!(
        ptext.contains("agentA"),
        "attached peer is visible via weave_peers: {ptext:?}"
    );

    // Failure: empty identity with no server default -> isError, nothing persisted.
    let (eerr, etext) = mcp.call_tool("weave_attach", serde_json::json!({"me": ""}));
    assert!(
        eerr,
        "empty identity must be an isError, not a silent persist: {etext}"
    );

    // Failure: oversized identity -> isError (MAX_IDENT_LEN cap).
    let huge = "x".repeat(100_000);
    let (herr, htext) = mcp.call_tool("weave_attach", serde_json::json!({"me": huge}));
    assert!(herr, "oversized identity must be an isError: {htext}");

    mcp.shutdown();
}

/// `weave_connect` verdicts over MCP:
/// - to an injectable peer (a `screen` peer, which has no liveness probe so the
///   fail-open verdict is `Live` regardless of any installed mux) -> "live",
///   `isError=false`;
/// - to a `mux=none` peer -> "not injectable" + will-queue, `isError=false`
///   (graceful, NOT an error);
/// - to a non-existent peer -> `isError=true` (the only hard failure).
#[test]
fn mcp_connect_verdicts_and_failure_path() {
    let db = TestDb::new();

    // Seed an injectable screen peer via the CLI (STY -> screen mux, no probe).
    let (ok, _o, _e) = common::run_stdin_full(
        &db,
        &["register", "--name", "screenpeer"],
        "",
        None,
        &[("STY", "1234.pts-0.host")],
    );
    assert!(ok, "register screen peer failed");
    // And a non-injectable peer.
    run_ok(&db, &["register", "--name", "queued"]);

    let mut mcp = McpServer::spawn(&db);

    // Live verdict (screen is injectable; no probe -> fail-open Live), not an error.
    let (lerr, ltext) = mcp.call_tool("weave_connect", serde_json::json!({"to": "screenpeer"}));
    assert!(!lerr, "connect to a live peer is not an error: {ltext}");
    assert!(
        ltext.contains("is live"),
        "connect reports live verdict: {ltext:?}"
    );

    // mux=none peer: not injectable, will queue, isError=false (graceful).
    let (qerr, qtext) = mcp.call_tool("weave_connect", serde_json::json!({"to": "queued"}));
    assert!(
        !qerr,
        "connect to a non-injectable peer must NOT be an error: {qtext}"
    );
    assert!(
        qtext.contains("not injectable") && qtext.contains("delivery will be queued"),
        "connect reports graceful queue fallback: {qtext:?}"
    );

    // Non-existent peer: the only hard failure -> isError.
    let (nerr, ntext) = mcp.call_tool("weave_connect", serde_json::json!({"to": "ghost"}));
    assert!(
        nerr,
        "connect to a non-existent peer must be isError: {ntext}"
    );
    assert!(
        ntext.contains("No registered peer 'ghost'"),
        "clear not-found error: {ntext:?}"
    );

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Tier-1 federation: read-only multi-store aggregation (WEAVE_PEER_DBS).
// ---------------------------------------------------------------------------

/// A peer registered in store B is visible in `weave peers` here when B is
/// configured via `WEAVE_PEER_DBS`, and the foreign row is origin-tagged
/// (`origin` = B's basename, `foreign` = true) in `--json`; the local row keeps
/// `origin:"local"`/`foreign:false` — the additive keys, not a reshape.
#[test]
fn federation_peers_union_origin_tagged() {
    let local = TestDb::new();
    let foreign = TestDb::new();

    // A local peer in store A and a distinct peer in store B.
    run_ok(&local, &["register", "--name", "here"]);
    run_ok(&foreign, &["register", "--name", "there"]);

    let foreign_path = foreign.path_str();
    let out = run_ok_env(
        &local,
        &["peers", "--json"],
        &[("WEAVE_PEER_DBS", &foreign_path)],
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("peers --json parses");
    let arr = v.as_array().expect("peers --json is an array");

    let here = arr
        .iter()
        .find(|p| p["name"] == "here")
        .expect("local peer present");
    assert_eq!(here["origin"], "local", "local row tagged local");
    assert_eq!(here["foreign"], false, "local row is not foreign");

    let there = arr
        .iter()
        .find(|p| p["name"] == "there")
        .expect("foreign peer surfaced via federation");
    assert_eq!(there["foreign"], true, "foreign row tagged foreign");
    let label = there["origin"].as_str().unwrap_or("");
    assert!(
        label.ends_with(".db") && label != "local",
        "foreign origin is the store basename, got {label:?}"
    );
    // The existing pre-Tier-1 keys are still present on every row.
    for key in ["name", "mux", "target", "alive", "injectable"] {
        assert!(there.get(key).is_some(), "row keeps key {key:?}: {there}");
    }
}

/// A session living only in store B surfaces in `weave sessions` here when B is
/// federated, origin-tagged foreign — and its unread is NOT summed into a local
/// session of the same name (Tier-1 has no cross-store inbox).
#[test]
fn federation_sessions_union_origin_tagged() {
    let local = TestDb::new();
    let foreign = TestDb::new();

    // Store B gets an unread session for "bob".
    run_ok(
        &foreign,
        &["send", "--from", "a", "--to", "bob", "--body", "hi-B"],
    );
    // Store A gets a different session "carol".
    run_ok(
        &local,
        &["send", "--from", "x", "--to", "carol", "--body", "hi-A"],
    );

    let foreign_path = foreign.path_str();
    let out = run_ok_env(
        &local,
        &["sessions", "--json"],
        &[("WEAVE_PEER_DBS", &foreign_path)],
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("sessions --json parses");
    let arr = v.as_array().expect("sessions --json is an array");

    let carol = arr
        .iter()
        .find(|s| s["name"] == "carol")
        .expect("local session present");
    assert_eq!(carol["foreign"], false);

    let bob = arr
        .iter()
        .find(|s| s["name"] == "bob")
        .expect("foreign session surfaced via federation");
    assert_eq!(bob["foreign"], true, "foreign session tagged foreign");
    assert_eq!(
        bob["unread"], 1,
        "foreign session keeps its own unread, not summed"
    );
}

/// Regression guard: with `WEAVE_PEER_DBS` UNSET the federated `peers --json` is
/// the single-store shape — every row tagged `origin:"local"`/`foreign:false`,
/// and no spurious foreign rows. The default path is identical-to-today plus the
/// two additive keys.
#[test]
fn federation_default_empty_is_local_only_shape() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "solo"]);

    let out = run_ok(&db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("peers --json parses");
    let arr = v.as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "only the local peer, no spurious foreign rows"
    );
    assert_eq!(arr[0]["name"], "solo");
    assert_eq!(arr[0]["origin"], "local");
    assert_eq!(arr[0]["foreign"], false);

    // Plain-text default output carries NO `(via ...)` tag for a local-only listing.
    let text = run_ok(&db, &["peers"]);
    assert!(
        !text.contains("(via "),
        "local-only listing must not emit a federation via-tag: {text}"
    );
}

/// Failure isolation: a nonexistent / non-weave junk path in `WEAVE_PEER_DBS` is
/// skipped (stderr note), the local listing still succeeds, and the exit code is
/// unaffected. stdout carries the local peer and no skip noise.
#[test]
fn federation_bad_store_is_skipped_not_fatal() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    // Two bad entries: a path that does not exist, and a real file that is not a
    // weave store (junk bytes).
    let junk = std::env::temp_dir().join(format!("weave-junk-{}.db", std::process::id()));
    std::fs::write(&junk, b"this is not a sqlite database at all").unwrap();
    let missing = std::env::temp_dir().join(format!("weave-missing-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&missing);

    let list = format!("{},{}", missing.display(), junk.display());
    let (ok, out, err) = run_env(&local, &["peers"], &[("WEAVE_PEER_DBS", &list)]);
    assert!(ok, "a bad federated store must not change the exit code");
    assert!(out.contains("here"), "local peer still listed: {out}");
    assert!(
        !out.contains("skipping federated store"),
        "skip notes must go to stderr, never stdout: {out}"
    );
    assert!(
        err.contains("skipping federated store"),
        "a bad store is diagnosed on stderr: {err}"
    );

    let _ = std::fs::remove_file(&junk);
}

/// MCP: `weave_peers`/`weave_sessions` reflect a federated peer/session from a
/// configured extra store, origin-tagged. A bad extra store mixed in still
/// yields a SUCCESSFUL tool result (`isError:false`) — federation degradation is
/// not a tool error. Confirms the CLI and MCP agree on the same federated view.
#[test]
fn mcp_peers_and_sessions_reflect_federation() {
    let local = TestDb::new();
    let foreign = TestDb::new();

    run_ok(&foreign, &["register", "--name", "fedpeer"]);
    run_ok(
        &foreign,
        &["send", "--from", "a", "--to", "fedsess", "--body", "hi"],
    );
    run_ok(&local, &["register", "--name", "localpeer"]);

    // Mix a real foreign store with a bad path: the tool must still succeed.
    let bad = std::env::temp_dir().join(format!("weave-mcp-nope-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&bad);
    let list = format!("{},{}", foreign.path_str(), bad.display());

    let mut mcp = McpServer::spawn_env(&local, &[("WEAVE_PEER_DBS", &list)]);

    let (perr, ptext) = mcp.call_tool("weave_peers", serde_json::json!({}));
    assert!(
        !perr,
        "weave_peers with a bad extra store is not an error: {ptext}"
    );
    assert!(ptext.contains("localpeer"), "local peer present: {ptext}");
    assert!(
        ptext.contains("fedpeer"),
        "federated peer surfaced in MCP weave_peers: {ptext}"
    );
    assert!(
        ptext.contains("(via "),
        "foreign peer is origin-tagged in MCP text: {ptext}"
    );

    let (serr, stext) = mcp.call_tool("weave_sessions", serde_json::json!({}));
    assert!(
        !serr,
        "weave_sessions with a bad extra store is not an error: {stext}"
    );
    assert!(
        stext.contains("fedsess"),
        "federated session surfaced in MCP weave_sessions: {stext}"
    );

    // weave_doctor reports the federation store count (configured/ok/skipped).
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "weave_doctor is not an error: {dtext}");
    assert!(
        dtext.contains("extra store"),
        "doctor reports the federation store count: {dtext}"
    );

    mcp.shutdown();
    let _ = std::fs::remove_file(&bad);
}
