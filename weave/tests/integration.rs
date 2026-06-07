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

use common::{run, run_env, run_hook, run_in_cwd, run_ok, run_ok_env, McpServer, TestDb};
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

/// P5: the two new presence tools are advertised, behave self-only, and the failure
/// paths are clean (NOT a panic, NEVER a silent persist): set_turn_state rejects a
/// bad enum value with isError; set_description truncates oversized text (never an
/// error); both surface on whoami/peers. stdout stays JSON-RPC only.
#[test]
fn mcp_presence_tools_set_and_fail_cleanly() {
    let db = TestDb::new();
    // Pin an explicit identity so whoami/setters resolve a stable caller row.
    let mut mcp = McpServer::spawn_full(&db, &["mcp", "--session", "p5mcp"], &[], None);

    // tools/list advertises BOTH new tools.
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("tools/list returns a tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    for expected in ["weave_set_description", "weave_set_turn_state"] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing {expected}; got {names:?}"
        );
    }

    // Register the caller's own peer row first: the setters are UPDATE-only on an
    // existing row (a never-registered identity is a 0-row no-op by design).
    let (is_err, _t) = mcp.call_tool("weave_attach", serde_json::json!({}));
    assert!(!is_err, "weave_attach should register the caller's row");

    // Happy: set_turn_state working.
    let (is_err, t) = mcp.call_tool(
        "weave_set_turn_state",
        serde_json::json!({"state": "working"}),
    );
    assert!(!is_err, "set_turn_state working should succeed: {t}");

    // FAILURE PATH: a bad state is isError (enum-reject), not a panic, not persisted.
    let (is_err, t) = mcp.call_tool(
        "weave_set_turn_state",
        serde_json::json!({"state": "bogus-state"}),
    );
    assert!(is_err, "an unknown turn_state must be isError: {t}");

    // Happy: set_description; oversized truncates rather than errors.
    let huge = "z".repeat(5000);
    let (is_err, t) = mcp.call_tool(
        "weave_set_description",
        serde_json::json!({"description": huge}),
    );
    assert!(
        !is_err,
        "oversized description truncates (never errors): {t}"
    );
    let (is_err, t) = mcp.call_tool(
        "weave_set_description",
        serde_json::json!({"description": "reviewing PR #23"}),
    );
    assert!(!is_err, "set_description should succeed: {t}");
    assert!(
        t.contains("reviewing PR #23"),
        "echoes the stored view: {t}"
    );

    // whoami ALWAYS surfaces turn_state + description for the caller.
    let (is_err, who) = mcp.call_tool("weave_whoami", serde_json::json!({}));
    assert!(!is_err, "whoami should not error: {who}");
    assert!(
        who.contains("turn_state: working"),
        "whoami surfaces turn_state: {who}"
    );
    assert!(
        who.contains("description: reviewing PR #23"),
        "whoami surfaces description: {who}"
    );

    // peers surfaces the compact marker + quoted description.
    let (is_err, peers) = mcp.call_tool("weave_peers", serde_json::json!({}));
    assert!(!is_err, "weave_peers should not error: {peers}");
    assert!(
        peers.contains("[working]") && peers.contains("\"reviewing PR #23\""),
        "peers surfaces the presence markers: {peers}"
    );

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

// MCP stdio has no per-call `--from`/`--me` flag. With neither `--session` nor
// `cfg.session` set, the server identity must fall back to the SAME basename(cwd)
// identity the CLI's resolve_me() uses, so tools resolve a caller instead of
// erroring `'from' is required`. We exercise that by running the server in a temp
// dir whose basename is a known valid session name and confirming weave_whoami
// reports it (and that a `from`-requiring tool, weave_send, succeeds with no
// explicit identity).
#[test]
fn mcp_stdio_identity_falls_back_to_basename_cwd() {
    let db = TestDb::new();
    // A unique temp dir whose *basename* is a valid identity. scrub_env clears
    // WEAVE_SESSION and points config at an empty dir, so neither --session nor
    // cfg.session is set; only basename(cwd) can supply the identity.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let me = format!("weave-peer-{pid}-{nanos}");
    let dir = std::env::temp_dir().join(&me);

    let mut mcp = McpServer::spawn_full(&db, &["mcp"], &[], Some(&dir));

    // weave_whoami must report the basename-derived identity (NOT "(unset ...)").
    let (is_err, who) = mcp.call_tool("weave_whoami", serde_json::json!({}));
    assert!(!is_err, "weave_whoami should not be an error: {who}");
    assert!(
        who.contains(&format!("identity:   {me}")),
        "whoami should report basename(cwd) identity {me:?}; got: {who:?}"
    );

    // And a `from`-requiring tool must succeed WITHOUT an explicit identity,
    // i.e. it must NOT error `'from' is required`.
    let (is_err, send_text) = mcp.call_tool(
        "weave_send",
        serde_json::json!({"to": "envctl", "body": "hi from fallback"}),
    );
    assert!(
        !is_err,
        "weave_send with no explicit `from` should succeed via basename fallback, \
         not error: {send_text}"
    );
    assert!(
        !send_text.contains("'from' is required"),
        "weave_send must not report \"'from' is required\": {send_text:?}"
    );

    mcp.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

// Regression: an explicit `--session <name>` must still win over the basename(cwd)
// fallback (the new `.or_else` is last). Run the server with --session set AND in a
// differently-named cwd; whoami must report the explicit name, not the basename.
#[test]
fn mcp_stdio_explicit_session_beats_basename_fallback() {
    let db = TestDb::new();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let cwd_name = format!("weave-cwd-{pid}-{nanos}");
    let dir = std::env::temp_dir().join(&cwd_name);
    let explicit = "explicit-session";

    let mut mcp = McpServer::spawn_full(&db, &["mcp", "--session", explicit], &[], Some(&dir));

    let (is_err, who) = mcp.call_tool("weave_whoami", serde_json::json!({}));
    assert!(!is_err, "weave_whoami should not be an error: {who}");
    assert!(
        who.contains(&format!("identity:   {explicit}")),
        "explicit --session must win over basename(cwd); got: {who:?}"
    );
    assert!(
        !who.contains(&cwd_name),
        "basename(cwd) {cwd_name:?} must NOT override an explicit --session: {who:?}"
    );

    mcp.shutdown();
    std::fs::remove_dir_all(&dir).ok();
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

// ─────────────────────── P5: rich presence (turn_state + description) ───────────

/// The full hook turn_state lifecycle surfaces in `weave peers` (human): session ⇒
/// pending, prompt ⇒ working, stop ⇒ idle (non-noisy markers), notification ⇒
/// awaiting-input. Each transition is driven through the compiled binary's hook arm.
#[test]
fn hook_turn_state_transitions_surface_in_peers() {
    let db = TestDb::new();
    // SessionStart ⇒ pending_first_turn (the [pending] marker).
    let (ok, _o, _e) = run_hook(&db, "session", r#"{"cwd":"/proj/p5a"}"#);
    assert!(ok);
    let peers = run_ok(&db, &["peers"]);
    assert!(peers.contains("p5a"), "peer registered: {peers}");
    assert!(
        peers.contains("[pending]"),
        "session hook ⇒ pending marker: {peers}"
    );

    // UserPromptSubmit ⇒ working.
    let (ok, _o, _e) = run_hook(&db, "prompt", r#"{"cwd":"/proj/p5a"}"#);
    assert!(ok);
    assert!(
        run_ok(&db, &["peers"]).contains("[working]"),
        "prompt hook ⇒ working marker"
    );

    // Stop ⇒ idle (NON-noisy: no marker, line back to baseline).
    let (ok, _o, _e) = run_hook(&db, "stop", r#"{"cwd":"/proj/p5a"}"#);
    assert!(ok);
    let after_stop = run_ok(&db, &["peers"]);
    assert!(
        !after_stop.contains("[working]")
            && !after_stop.contains("[pending]")
            && !after_stop.contains("[awaiting-input]"),
        "stop hook ⇒ idle renders NO turn_state marker (non-noisy): {after_stop}"
    );

    // Notification ⇒ awaiting-input.
    let (ok, _o, _e) = run_hook(&db, "notification", r#"{"cwd":"/proj/p5a"}"#);
    assert!(ok);
    assert!(
        run_ok(&db, &["peers"]).contains("[awaiting-input]"),
        "notification hook ⇒ awaiting-input marker"
    );
}

/// `weave describe` sets a self-only description that surfaces in `peers`, and
/// `--json` peers/whoami carry the new presence keys. (TTL expiry is unit-tested at
/// the store seam; integration confirms the wiring + JSON shape.)
#[test]
fn describe_surfaces_in_peers_and_json_carries_presence_keys() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "p5b"]);
    let out = run_ok(&db, &["describe", "reviewing PR #23", "--me", "p5b"]);
    assert!(
        out.contains("description set for 'p5b'") && out.contains("reviewing PR #23"),
        "describe echoes the stored view: {out}"
    );
    let peers = run_ok(&db, &["peers"]);
    assert!(
        peers.contains("\"reviewing PR #23\""),
        "description surfaces (quoted) in peers human output: {peers}"
    );

    // status sets turn_state self-only; surfaces as a marker.
    run_ok(&db, &["status", "working", "--me", "p5b"]);
    assert!(
        run_ok(&db, &["peers"]).contains("[working]"),
        "status working ⇒ marker"
    );

    // --json peers carries additive keys, no error.
    let json = run_ok(&db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("peers --json parses");
    let p = v
        .as_array()
        .and_then(|a| a.iter().find(|p| p["name"] == "p5b"))
        .expect("p5b in peers --json");
    assert_eq!(p["turn_state"], "working", "json turn_state key: {p}");
    assert_eq!(p["description"], "reviewing PR #23", "json description key");
    assert!(
        p.get("description_ts").is_some(),
        "json description_ts key present"
    );
}

/// A bad turn_state via `weave status` is a clean error (enum-reject), NOT a panic,
/// and writes nothing.
#[test]
fn status_rejects_unknown_turn_state() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "p5c"]);
    let (ok, _out, err) = run(&db, &["status", "totally-bogus", "--me", "p5c"]);
    assert!(!ok, "an unknown turn_state must fail cleanly");
    assert!(
        err.to_lowercase().contains("unknown turn state"),
        "error names the bad state: {err}"
    );
    // No marker leaked into the listing.
    let peers = run_ok(&db, &["peers"]);
    assert!(
        !peers.contains("[working]") && !peers.contains("[pending]"),
        "rejected state never surfaced: {peers}"
    );
}

/// BACKWARD-COMPAT REGRESSION: a peer with NO turn_state/description set renders the
/// SAME human line as pre-P5 — no presence marker tokens appear anywhere in
/// peers/sessions/scan default output, and the peers line still matches the legacy
/// format exactly (only the presence-marker insertion points differ, and they insert
/// the empty string). Proves the non-noisy default.
#[test]
fn unset_presence_default_output_is_byte_identical() {
    let db = TestDb::new();
    // Register via a hook session (a realistic fresh peer) WITHOUT any describe/status.
    let (ok, _o, _e) = run_hook(&db, "session", r#"{"cwd":"/proj/legacy_view"}"#);
    assert!(ok);
    // After the session hook the peer is pending_first_turn — but that DOES surface a
    // marker by design. The regression target is a peer with turn_state idle/unknown
    // AND no description: drive it to idle (stop) to reach the "no marker" baseline.
    run_hook(&db, "stop", r#"{"cwd":"/proj/legacy_view"}"#);

    for cmd in [vec!["peers"], vec!["sessions"], vec!["scan"]] {
        let out = run_ok(&db, &cmd);
        for marker in ["[working]", "[awaiting-input]", "[pending]"] {
            assert!(
                !out.contains(marker),
                "default `weave {}` must not emit {marker} for an idle/no-description peer: {out}",
                cmd.join(" ")
            );
        }
        // No stray description quoting from an empty description.
        assert!(
            !out.contains(" \"\""),
            "an empty description must render NOTHING (no quotes): {out}"
        );
    }

    // The peers line still has the legacy bracket shape: [presence] [reason] then the
    // mux bracket — with NO extra bracket inserted between them when idle/unknown.
    let peers = run_ok(&db, &["peers"]);
    assert!(peers.contains("legacy_view"), "peer present: {peers}");
    // The marker insertion point is "[reason]<here> [mux]"; for an idle peer it is
    // empty, so two consecutive "] [" sequences (reason]→[mux) survive unchanged.
    assert!(
        peers.contains("] ["),
        "legacy bracket adjacency preserved (no marker inserted): {peers}"
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
// Lifecycle hooks (weave hook session|prompt|stop|wake) — the Claude Code integration.
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
fn hook_wake_blocks_once_then_rearms_after_drain() {
    let db = TestDb::new();
    run_hook(&db, "session", r#"{"cwd":"/proj/gamma"}"#);
    run_ok(
        &db,
        &[
            "send",
            "--from",
            "x",
            "--to",
            "gamma",
            "--body",
            "wakepayload",
        ],
    );

    // Wake emits a structured block response the first time.
    let (_o1, out1, _e1) = run_hook(&db, "wake", r#"{"cwd":"/proj/gamma"}"#);
    assert!(
        out1.contains("\"decision\":\"block\"") && out1.contains("wakepayload"),
        "wake should block and include the unread body: {out1}"
    );

    // Repeated wake is silent until the unread backlog is drained.
    let (_o2, out2, _e2) = run_hook(&db, "wake", r#"{"cwd":"/proj/gamma"}"#);
    assert!(
        out2.trim().is_empty(),
        "second wake should be silent after the ack: {out2}"
    );

    // Prompt drains and marks read, so a newer message can wake again.
    let (_op, prompt_out, _ep) = run_hook(&db, "prompt", r#"{"cwd":"/proj/gamma"}"#);
    assert!(prompt_out.contains("wakepayload"));

    run_ok(
        &db,
        &[
            "send",
            "--from",
            "x",
            "--to",
            "gamma",
            "--body",
            "wakepayload2",
        ],
    );
    let (_o3, out3, _e3) = run_hook(&db, "wake", r#"{"cwd":"/proj/gamma"}"#);
    assert!(
        out3.contains("\"decision\":\"block\"") && out3.contains("wakepayload2"),
        "wake should re-arm for newer unread work: {out3}"
    );
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

    // Failure: oversized identity -> isError (MAX_IDENT_LEN cap), even with a
    // server default present.
    let huge = "x".repeat(100_000);
    let (herr, htext) = mcp.call_tool("weave_attach", serde_json::json!({"me": huge}));
    assert!(herr, "oversized identity must be an isError: {htext}");

    mcp.shutdown();

    // Failure: empty identity with NO server default -> isError, nothing persisted.
    // The server now falls back to basename(cwd) for its default identity, so to
    // exercise the genuine "no default" path we run it in a degenerate cwd ("/",
    // whose file_name() is None -> resolve_me yields "unknown" -> identity stays
    // unset). Then an empty `me` has nothing to fall back to and must error.
    let db2 = TestDb::new();
    let mut mcp2 = McpServer::spawn_full(&db2, &["mcp"], &[], Some(Path::new("/")));
    let (eerr, etext) = mcp2.call_tool("weave_attach", serde_json::json!({"me": ""}));
    assert!(
        eerr,
        "empty identity with no server default must be an isError, not a silent persist: {etext}"
    );
    mcp2.shutdown();
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

// ---------------------------------------------------------------------------
// Tier-2 cross-store delivery (2a outbox + authorized send, 2b pull/commit/dedup)
//
// Two temp stores A (sender) and B (receiver). A `weave send --to-store <B>`
// deposits an Intent into A's OWN outbox; B with `WEAVE_PULL_FROM=<A>` pulls and
// commits it into B's inbox. Owner-only-writes, allowlist, idempotency, failure
// isolation and the local-send regression are exercised black-box through the
// compiled binary.
// ---------------------------------------------------------------------------

/// A cross-store `send --to-store` writes an Intent into the SENDER's outbox and
/// creates NO inbox row in the sender; `weave outbox` lists the pending intent.
/// With `WEAVE_PULL_FROM=<A>` the receiver pulls it into its own inbox with a
/// receiver-assigned id/ts and the sender's `from` attribution. A second pull is
/// idempotent (no duplicate). The original local store stays untouched as a row.
#[test]
fn tier2_cross_store_send_outbox_pull_and_idempotency() {
    let a = TestDb::new(); // sender store
    let b = TestDb::new(); // receiver store

    // A enqueues a directed cross-store intent for "bob" living in store B.
    let sent = run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "hi",
            "--body",
            "hello from another store",
            "--to-store",
            &b.path_str(),
        ],
    );
    assert!(
        sent.contains("queued intent"),
        "cross-store send reports a queued intent, got: {sent}"
    );

    // The intent lives in A's OUTBOX, not A's inbox.
    let a_outbox = run_ok(&a, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&a_outbox).expect("outbox --json parses");
    assert_eq!(
        ov["outbox"].as_array().map(|x| x.len()),
        Some(1),
        "exactly one pending intent in A's outbox: {a_outbox}"
    );
    assert_eq!(ov["outbox"][0]["to"], "bob");
    assert_eq!(ov["outbox"][0]["from"], "alice");
    assert_eq!(ov["outbox"][0]["body"], "hello from another store");

    // A's own inbox (for bob) has NOTHING: the sender never wrote a local row.
    let a_inbox = run_ok(&a, &["inbox", "--me", "bob", "--json", "--peek"]);
    let aiv: serde_json::Value = serde_json::from_str(&a_inbox).expect("inbox --json parses");
    assert_eq!(
        aiv["messages"].as_array().map(|x| x.len()),
        Some(0),
        "cross-store send must NOT create a local inbox row in the sender: {a_inbox}"
    );

    // B pulls from A (allow-listed via WEAVE_PULL_FROM). The message appears in
    // B's inbox, attributed to A's `from`.
    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[("WEAVE_PULL_FROM", &a.path_str())],
    );
    assert!(
        pull.contains("pulled 1 message"),
        "first pull commits exactly one message: {pull}"
    );

    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox --json parses");
    assert_eq!(
        biv["messages"].as_array().map(|x| x.len()),
        Some(1),
        "B's inbox has exactly one delivered message: {b_inbox}"
    );
    assert_eq!(biv["messages"][0]["sender"], "alice", "from-attribution");
    assert_eq!(biv["messages"][0]["body"], "hello from another store");
    // The receiver assigns its own id (>0) at commit time (anchored to B).
    assert!(
        biv["messages"][0]["id"].as_i64().unwrap_or(0) > 0,
        "B assigns its own local id"
    );

    // IDEMPOTENCY: a second pull with no new intents delivers nothing new.
    let pull2 = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[("WEAVE_PULL_FROM", &a.path_str())],
    );
    assert!(
        pull2.contains("pulled 0 message"),
        "re-pull with no new intents commits nothing: {pull2}"
    );
    let b_inbox2 = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv2: serde_json::Value = serde_json::from_str(&b_inbox2).expect("inbox --json parses");
    assert_eq!(
        biv2["messages"].as_array().map(|x| x.len()),
        Some(1),
        "re-pull must NOT duplicate the delivered message: {b_inbox2}"
    );
}

/// Two intents with the SAME content but different outbox ids both deliver (the
/// dedup key is the source outbox id, not the content). A newly-enqueued intent
/// after a pull is delivered on the next pull (the high-water cursor only blocks
/// already-committed ids, never future ones).
#[test]
fn tier2_dedup_keyed_on_intent_id_not_content() {
    let a = TestDb::new();
    let b = TestDb::new();

    // Two identical-content intents (distinct outbox ids).
    for _ in 0..2 {
        run_ok(
            &a,
            &[
                "send",
                "--from",
                "alice",
                "--to",
                "bob",
                "--body",
                "same body",
                "--to-store",
                &b.path_str(),
            ],
        );
    }

    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[("WEAVE_PULL_FROM", &a.path_str())],
    );
    assert!(
        pull.contains("pulled 2 message"),
        "two same-content distinct-id intents both deliver: {pull}"
    );
    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(
        v["messages"].as_array().map(|x| x.len()),
        Some(2),
        "both identical-content messages land in B's inbox: {inbox}"
    );

    // Enqueue a THIRD intent after the first pull; the next pull delivers only it.
    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "later message",
            "--to-store",
            &b.path_str(),
        ],
    );
    let pull2 = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[("WEAVE_PULL_FROM", &a.path_str())],
    );
    assert!(
        pull2.contains("pulled 1 message"),
        "only the new intent is delivered on the next pull: {pull2}"
    );
}

/// Allowlist: a source NOT in the receiver's `pull_from` is never opened, so it
/// cannot deliver. With no `WEAVE_PULL_FROM`, a pull commits nothing.
#[test]
fn tier2_unlisted_source_never_delivers() {
    let a = TestDb::new();
    let b = TestDb::new();

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "should not arrive",
            "--to-store",
            &b.path_str(),
        ],
    );

    // No WEAVE_PULL_FROM at all => A is not allow-listed => nothing delivered.
    let pull = run_ok(&b, &["pull", "--me", "bob"]);
    assert!(
        pull.contains("pulled 0 message") && pull.contains("from 0 source"),
        "with no pull_from configured, nothing is pulled: {pull}"
    );
    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(
        v["messages"].as_array().map(|x| x.len()),
        Some(0),
        "an unlisted source never delivers into B's inbox: {inbox}"
    );
}

/// An intent addressed to someone OTHER than the puller is never committed even
/// when the source IS allow-listed (commit additionally requires `to == me`).
#[test]
fn tier2_misaddressed_intent_not_committed() {
    let a = TestDb::new();
    let b = TestDb::new();

    // Addressed to carol, but bob pulls.
    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "carol",
            "--body",
            "not for bob",
            "--to-store",
            &b.path_str(),
        ],
    );
    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[("WEAVE_PULL_FROM", &a.path_str())],
    );
    assert!(
        pull.contains("pulled 0 message"),
        "an intent addressed to carol is not committed to bob: {pull}"
    );
    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"].as_array().map(|x| x.len()), Some(0));
}

/// Failure isolation: an unreadable/nonexistent/junk pull source is skipped and
/// the good source still delivers; the pull exits 0.
#[test]
fn tier2_bad_source_is_skipped_good_source_delivers() {
    let a = TestDb::new();
    let b = TestDb::new();

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "delivered despite a bad sibling source",
            "--to-store",
            &b.path_str(),
        ],
    );

    // A junk (non-weave) file source.
    let junk = std::env::temp_dir().join(format!("weave-junk-{}.db", std::process::id()));
    std::fs::write(&junk, b"this is not a sqlite database at all").unwrap();
    let missing = std::env::temp_dir().join(format!("weave-missing-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&missing);

    let list = format!("{},{},{}", missing.display(), junk.display(), a.path_str());
    let (ok, out, _err) = run_env(&b, &["pull", "--me", "bob"], &[("WEAVE_PULL_FROM", &list)]);
    assert!(ok, "pull exits 0 even with bad sources: {out}");
    assert!(
        out.contains("pulled 1 message"),
        "the good source still delivers: {out}"
    );
    assert!(
        out.contains("skipped"),
        "bad sources reported as skipped: {out}"
    );

    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"].as_array().map(|x| x.len()), Some(1));

    let _ = std::fs::remove_file(&junk);
}

/// Local regression: a purely-local `weave send` (no `--to-store`) is unchanged —
/// a direct inbox row, and NOTHING in the outbox.
#[test]
fn tier2_local_send_unchanged_no_outbox() {
    let db = TestDb::new();
    let out = run_ok(
        &db,
        &["send", "--from", "a", "--to", "b", "--body", "local hi"],
    );
    assert!(
        out.contains("sent #") && !out.contains("queued intent"),
        "a local send is a direct send, not an intent: {out}"
    );
    // Inbox has the row.
    let inbox = run_ok(&db, &["inbox", "--me", "b", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"][0]["body"], "local hi");
    // Outbox is empty (no cross-store intent was created).
    let outbox = run_ok(&db, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&outbox).expect("outbox parses");
    assert_eq!(
        ov["outbox"].as_array().map(|x| x.len()),
        Some(0),
        "a local send creates NO outbox intent: {outbox}"
    );
}

/// Cross-store broadcast is rejected at the routing seam (Tier-2 is directed-only).
#[test]
fn tier2_cross_store_broadcast_rejected() {
    let a = TestDb::new();
    let b = TestDb::new();
    let (ok, _out, err) = run(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "all",
            "--body",
            "broadcast attempt",
            "--to-store",
            &b.path_str(),
        ],
    );
    assert!(
        !ok,
        "cross-store broadcast must be rejected (non-zero exit)"
    );
    assert!(
        err.contains("broadcast"),
        "rejection mentions broadcast: {err}"
    );
    // Nothing queued.
    let outbox = run_ok(&a, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&outbox).expect("outbox parses");
    assert_eq!(ov["outbox"].as_array().map(|x| x.len()), Some(0));
}

/// Tier-2 phase 2c, DEFAULT-ON consent nudge: when B pulls an allow-listed
/// cross-store message, B ALSO fires the paste-safe CONTENT-FREE nudge into B's
/// OWN registered pane — by default, with no `inject_pulled` set. The fake tmux
/// records a `send-keys` of the fixed ping (never the body). The toggle-off case
/// (`WEAVE_INJECT_PULLED=false`) is pure queue-only: the message still delivers
/// but NO keystroke is recorded.
#[test]
fn tier2_pulled_message_nudges_own_pane_by_default() {
    let a = TestDb::new(); // sender store
    let b = TestDb::new(); // receiver store
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    // A enqueues a directed cross-store intent for "bob" in store B.
    let sent = run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "secret payload",
            "--to-store",
            &b.path_str(),
        ],
    );
    assert!(sent.contains("queued intent"), "intent queued: {sent}");

    // B registers its OWN session "bob" as an injectable tmux pane %3.
    let reg = weave_with_fake_path(
        &b,
        &fake_dir,
        &[("TMUX_PANE", "%3")],
        &["register", "--name", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    // B pulls from A (allow-listed). DEFAULT-ON consent ⇒ a nudge is fired into
    // B's own pane %3 via the fake tmux. No WEAVE_INJECT_PULLED set ⇒ default true.
    let pull = weave_with_fake_path(
        &b,
        &fake_dir,
        &[("WEAVE_PULL_FROM", &a.path_str())],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull");
    assert!(
        pull.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull.stdout).contains("pulled 1 message"),
        "pull commits the one message: {}",
        String::from_utf8_lossy(&pull.stdout)
    );

    let logged = read_log_with_retries(&log);
    // The consent nudge fired into B's own pane %3 ...
    assert!(
        logged.contains("send-keys") && logged.contains("-t %3"),
        "default-on consent nudge should send-keys to B's own pane %3:\n{logged}"
    );
    // ... and it is the CONTENT-FREE ping, NOT the message body.
    assert!(
        logged.contains("check your inbox"),
        "the nudge is the content-free ping:\n{logged}"
    );
    assert!(
        !logged.contains("secret payload"),
        "the message body must NEVER appear in the keystrokes:\n{logged}"
    );

    // The message was still delivered into B's inbox (delivery is independent of
    // the nudge).
    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox parses");
    assert_eq!(biv["messages"].as_array().map(|x| x.len()), Some(1));
}

/// Tier-2 phase 2c, the single OFF-SWITCH: `WEAVE_INJECT_PULLED=false` ⇒ a pulled
/// message is committed (delivered to the inbox) but NO nudge is fired (pure
/// queue-only). The fake tmux records no `send-keys`.
#[test]
fn tier2_inject_pulled_false_is_queue_only() {
    let a = TestDb::new();
    let b = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "queue only please",
            "--to-store",
            &b.path_str(),
        ],
    );

    let reg = weave_with_fake_path(
        &b,
        &fake_dir,
        &[("TMUX_PANE", "%4")],
        &["register", "--name", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(reg.status.success(), "register failed");

    // Pull with the consent master toggle OFF.
    let pull = weave_with_fake_path(
        &b,
        &fake_dir,
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("WEAVE_INJECT_PULLED", "false"),
        ],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull");
    assert!(
        pull.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull.stdout).contains("pulled 1 message"),
        "the message is still delivered: {}",
        String::from_utf8_lossy(&pull.stdout)
    );

    // Give any (erroneous) async inject a moment; then assert the log has NO
    // send-keys. A `has-session`/other probe may appear, but never a send-keys.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !logged.contains("send-keys"),
        "inject_pulled=false must be pure queue-only (no keystroke):\n{logged}"
    );

    // Delivery still happened.
    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox parses");
    assert_eq!(biv["messages"].as_array().map(|x| x.len()), Some(1));
}

/// Tier-2 phase 2c, ALLOWLIST NARROWING: with `allow_inject_from` set to a subset
/// of `pull_from`, a pulled message from a source that IS on `pull_from` but NOT in
/// `allow_inject_from` is delivered to the inbox yet NEVER triggers a keystroke;
/// a source that IS in the subset does inject. Proves the per-source inject gate is
/// honored caller-side, independent of (and after) the master toggle.
#[test]
fn tier2_allow_inject_from_narrows_to_subset() {
    let trusted = TestDb::new(); // on pull_from AND allow_inject_from -> injects
    let untrusted = TestDb::new(); // on pull_from but NOT allow_inject_from -> no nudge
    let b = TestDb::new(); // receiver

    // Each source enqueues a distinct directed intent for "bob" in store B.
    run_ok(
        &untrusted,
        &[
            "send",
            "--from",
            "mallory",
            "--to",
            "bob",
            "--body",
            "untrusted body",
            "--to-store",
            &b.path_str(),
        ],
    );
    run_ok(
        &trusted,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "trusted body",
            "--to-store",
            &b.path_str(),
        ],
    );

    // ---- Case 1: pull ONLY the untrusted source (on pull_from, NOT allow set) ----
    let log_u = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log_u);
    let fake_u = make_fake_tmux(&log_u);
    let reg = weave_with_fake_path(
        &b,
        &fake_u,
        &[("TMUX_PANE", "%5")],
        &["register", "--name", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let pull_u = weave_with_fake_path(
        &b,
        &fake_u,
        &[
            ("WEAVE_PULL_FROM", &untrusted.path_str()),
            // allow_inject_from narrows to ONLY the trusted store, so untrusted
            // delivers but is never inject-eligible.
            ("WEAVE_ALLOW_INJECT_FROM", &trusted.path_str()),
        ],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull (untrusted)");
    assert!(
        pull_u.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull_u.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull_u.stdout).contains("pulled 1 message"),
        "untrusted source still DELIVERS: {}",
        String::from_utf8_lossy(&pull_u.stdout)
    );
    // Give any (erroneous) inject a moment, then assert NO keystroke for untrusted.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let logged_u = std::fs::read_to_string(&log_u).unwrap_or_default();
    assert!(
        !logged_u.contains("send-keys"),
        "a source NOT in allow_inject_from must never inject:\n{logged_u}"
    );

    // ---- Case 2: pull the trusted source (on pull_from AND allow set) ----
    let log_t = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log_t);
    let fake_t = make_fake_tmux(&log_t);
    let pull_t = weave_with_fake_path(
        &b,
        &fake_t,
        &[
            ("WEAVE_PULL_FROM", &trusted.path_str()),
            ("WEAVE_ALLOW_INJECT_FROM", &trusted.path_str()),
        ],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull (trusted)");
    assert!(
        pull_t.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull_t.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull_t.stdout).contains("pulled 1 message"),
        "trusted source delivers: {}",
        String::from_utf8_lossy(&pull_t.stdout)
    );
    let logged_t = read_log_with_retries(&log_t);
    assert!(
        logged_t.contains("send-keys") && logged_t.contains("-t %5"),
        "a source in allow_inject_from DOES inject into B's own pane %5:\n{logged_t}"
    );
    assert!(
        logged_t.contains("check your inbox") && !logged_t.contains("trusted body"),
        "the inject is the content-free ping, never the body:\n{logged_t}"
    );

    // Both messages landed in B's inbox regardless of the inject gate.
    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox parses");
    assert_eq!(
        biv["messages"].as_array().map(|x| x.len()),
        Some(2),
        "both messages delivered (gate only suppresses the nudge): {b_inbox}"
    );
}

/// Tier-2 phase 2c, NON-INJECTABLE FALL-OPEN: a receiver whose own pane is `mux=none`
/// (no registered injectable pane) delivers a pulled allow-listed message with no
/// error and no injection — graceful degradation to queue-only. With `inject_pulled`
/// default-on, the nudge path must still fail open silently.
#[test]
fn tier2_non_injectable_receiver_falls_open_to_queue_only() {
    let a = TestDb::new();
    let b = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "no pane here",
            "--to-store",
            &b.path_str(),
        ],
    );

    // Register B's session "bob" with NO mux pane (no TMUX_PANE, fake mux on PATH
    // does not matter — without a pane env weave records mux=none).
    let reg = weave_with_fake_path(&b, &fake_dir, &[], &["register", "--name", "bob"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    // Pull with default-on consent. The receiver has no injectable pane, so the
    // nudge must fall open to queue-only with no error.
    let pull = weave_with_fake_path(
        &b,
        &fake_dir,
        &[("WEAVE_PULL_FROM", &a.path_str())],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull");
    assert!(
        pull.status.success(),
        "a non-injectable receiver must still pull cleanly (exit 0): {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull.stdout).contains("pulled 1 message"),
        "the message is delivered even without an injectable pane: {}",
        String::from_utf8_lossy(&pull.stdout)
    );

    // No keystroke was attempted (mux=none ⇒ not injectable ⇒ no send-keys).
    std::thread::sleep(std::time::Duration::from_millis(150));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !logged.contains("send-keys"),
        "a mux=none receiver must never produce a keystroke:\n{logged}"
    );

    // Delivery still happened.
    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox parses");
    assert_eq!(biv["messages"].as_array().map(|x| x.len()), Some(1));
}

/// Tier-2 phase 2c, MCP DRAIN parity: the MCP `weave_inbox` drain ALSO fires the
/// default-on consent nudge for a pulled allow-listed message — identical behavior
/// to the CLI `weave pull` drain. The fake tmux records the content-free ping into
/// B's own pane; the body never appears in the keystrokes.
#[test]
fn tier2_mcp_inbox_drain_nudges_own_pane_by_default() {
    let a = TestDb::new();
    let b = TestDb::new();
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let fake_dir = make_fake_tmux(&log);

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "mcp secret body",
            "--to-store",
            &b.path_str(),
        ],
    );

    // Register B's pane %6 via the CLI under the fake mux (the MCP server itself
    // does not auto-register).
    let reg = weave_with_fake_path(
        &b,
        &fake_dir,
        &[("TMUX_PANE", "%6")],
        &["register", "--name", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    // Spawn the MCP server with the fake mux on PATH + WEAVE_MUX_DIR and the pull
    // source. A `weave_inbox` call triggers the same default-on nudge as the CLI.
    let orig_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fake_dir.display(), orig_path);
    let fake_dir_str = fake_dir.display().to_string();
    let mut mcp = McpServer::spawn_env(
        &b,
        &[
            ("PATH", new_path.as_str()),
            ("WEAVE_MUX_DIR", fake_dir_str.as_str()),
            ("WEAVE_PULL_FROM", &a.path_str()),
        ],
    );

    let (err, text) = mcp.call_tool("weave_inbox", serde_json::json!({ "me": "bob" }));
    assert!(!err, "weave_inbox drain must not error: {text}");
    assert!(
        text.contains("mcp secret body"),
        "the pulled message is delivered in the same drain: {text}"
    );
    mcp.shutdown();

    let logged = read_log_with_retries(&log);
    assert!(
        logged.contains("send-keys") && logged.contains("-t %6"),
        "the MCP drain fires the default-on nudge into B's own pane %6:\n{logged}"
    );
    assert!(
        logged.contains("check your inbox"),
        "the MCP-drain nudge is the content-free ping:\n{logged}"
    );
    assert!(
        !logged.contains("mcp secret body"),
        "the body must NEVER reach the keystrokes on the MCP drain path:\n{logged}"
    );
}

/// Tier-2 phase 2c, NON-FATAL inject failure: when the registered pane refers to a
/// mux target whose live submission fails (the fake mux's `send-keys` exits non-zero),
/// the pull/drain still succeeds and the message stays in the inbox. A failed nudge
/// must never break delivery.
#[test]
fn tier2_inject_failure_is_non_fatal_to_delivery() {
    let a = TestDb::new();
    let b = TestDb::new();

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "delivered despite fail",
            "--to-store",
            &b.path_str(),
        ],
    );

    // A fake mux whose has-session/liveness probe succeeds (so the target is
    // considered alive) but whose send-keys FAILS. We build it inline: exit 0 for
    // everything EXCEPT send-keys, which exits 1.
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let dir = common::unique_db().with_extension("muxbin");
    std::fs::create_dir_all(&dir).expect("create fake-mux dir");
    let script = dir.join("tmux");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nfor a in \"$@\"; do\n  if [ \"$a\" = send-keys ]; then exit 1; fi\ndone\nexit 0\n",
        log.display()
    );
    std::fs::write(&script, body).expect("write fake tmux");
    let mut perms = std::fs::metadata(&script).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");

    let reg = weave_with_fake_path(
        &b,
        &dir,
        &[("TMUX_PANE", "%8")],
        &["register", "--name", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let pull = weave_with_fake_path(
        &b,
        &dir,
        &[("WEAVE_PULL_FROM", &a.path_str())],
        &["pull", "--me", "bob"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn pull");
    // The inject failed, but the drain must still exit 0 and report delivery.
    assert!(
        pull.status.success(),
        "a failed inject must NOT break the pull (exit 0): {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert!(
        String::from_utf8_lossy(&pull.stdout).contains("pulled 1 message"),
        "the message is committed even though the nudge failed: {}",
        String::from_utf8_lossy(&pull.stdout)
    );
    // Confirm the inject WAS attempted (the failing send-keys is recorded), proving
    // we exercised the failure path rather than skipping it.
    let logged = read_log_with_retries(&log);
    assert!(
        logged.contains("send-keys"),
        "the inject was attempted (and failed): {logged}"
    );

    // The message survives in B's inbox.
    let b_inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let biv: serde_json::Value = serde_json::from_str(&b_inbox).expect("inbox parses");
    assert_eq!(
        biv["messages"].as_array().map(|x| x.len()),
        Some(1),
        "delivery is independent of the nudge outcome: {b_inbox}"
    );
}

/// MCP `weave_send` cross-store routing: with `to_store` the tool returns a
/// success (`isError:false`) "Queued intent" result and the intent shows up via
/// `weave_outbox`. A broadcast cross-store send returns `isError`.
#[test]
fn mcp_weave_send_cross_store_routes_to_outbox() {
    let a = TestDb::new();
    let b = TestDb::new();
    let mut mcp = McpServer::spawn(&a);

    // Cross-store send -> queued intent, not an error.
    let (err, text) = mcp.call_tool(
        "weave_send",
        serde_json::json!({
            "from": "alice",
            "to": "bob",
            "body": "via mcp cross-store",
            "to_store": b.path_str(),
        }),
    );
    assert!(!err, "cross-store weave_send is not an error: {text}");
    assert!(
        text.contains("Queued intent"),
        "cross-store send reports a queued intent: {text}"
    );

    // weave_outbox lists the pending intent.
    let (oerr, otext) = mcp.call_tool("weave_outbox", serde_json::json!({}));
    assert!(!oerr, "weave_outbox is not an error: {otext}");
    assert!(
        otext.contains("bob") && otext.contains("via mcp cross-store"),
        "weave_outbox lists the queued intent: {otext}"
    );

    // Cross-store broadcast is rejected (isError).
    let (berr, btext) = mcp.call_tool(
        "weave_send",
        serde_json::json!({
            "from": "alice",
            "to": "all",
            "body": "no fan-out",
            "to_store": b.path_str(),
        }),
    );
    assert!(
        berr,
        "cross-store broadcast via weave_send must be an error: {btext}"
    );
    assert!(
        btext.contains("broadcast"),
        "error names broadcast: {btext}"
    );

    mcp.shutdown();
}

/// MCP `weave_send` cross-store failure path: a bad recipient identity (oversized)
/// with `to_store` returns `isError` and persists nothing in the outbox.
#[test]
fn mcp_weave_send_cross_store_bad_recipient_is_error() {
    let a = TestDb::new();
    let b = TestDb::new();
    let oversized = "n".repeat(5_000);
    let mut mcp = McpServer::spawn(&a);

    let (err, text) = mcp.call_tool(
        "weave_send",
        serde_json::json!({
            "from": "alice",
            "to": oversized,
            "body": "hi",
            "to_store": b.path_str(),
        }),
    );
    assert!(
        err,
        "an oversized cross-store recipient must be an isError result, not a panic: {text}"
    );

    // The bad intent was never persisted.
    let (_oerr, otext) = mcp.call_tool("weave_outbox", serde_json::json!({}));
    assert!(
        otext.contains("Outbox empty"),
        "a rejected cross-store send persists nothing: {otext}"
    );

    mcp.shutdown();
}

/// MCP inbox drain pulls cross-store intents: with `WEAVE_PULL_FROM` set, calling
/// `weave_inbox` on the receiver opportunistically pulls + commits the intent and
/// returns it in the same read (the pull-on-drain wiring).
#[test]
fn mcp_weave_inbox_pulls_cross_store_on_drain() {
    let a = TestDb::new();
    let b = TestDb::new();

    // A queues an intent for bob in B.
    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "delivered via mcp drain",
            "--to-store",
            &b.path_str(),
        ],
    );

    // B's MCP server with A as a pull source; reading bob's inbox drains the pull.
    let mut mcp = McpServer::spawn_env(&b, &[("WEAVE_PULL_FROM", &a.path_str())]);
    let (err, text) = mcp.call_tool("weave_inbox", serde_json::json!({"me": "bob"}));
    assert!(!err, "weave_inbox is not an error: {text}");
    assert!(
        text.contains("delivered via mcp drain"),
        "the cross-store intent is pulled and shown on the inbox drain: {text}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Tier-2 phase 2d — signed sender identity (only built with `--features sign`).
//
// These drive the REAL `--features sign` binary end-to-end through key files on
// disk: each actor gets its own isolated `XDG_CONFIG_HOME` (so the private key
// lands in a per-actor temp config dir, never the harness's), `weave key gen`
// writes the keypair, the public key is registered on the receiver, and a signed
// cross-store send is pulled and verified before commit.
// ---------------------------------------------------------------------------

/// A unique, isolated `XDG_CONFIG_HOME` dir for one signing actor. The private
/// key file (`<dir>/weave/ed25519.key`) is created under it; nothing the real
/// user owns is touched.
#[cfg(feature = "sign")]
fn unique_config_home() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("weave-sign-cfg-{pid}-{n}-{nanos}"));
    std::fs::create_dir_all(&d).expect("create temp config home");
    d
}

/// Parse the `public key:  <hex>` line emitted by `weave key gen`.
#[cfg(feature = "sign")]
fn pubkey_from_gen(out: &str) -> String {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("public key:"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| panic!("`weave key gen` did not print a public key:\n{out}"))
}

/// Full signed cross-store flow: A `key gen`, register A's pubkey on B, A sends a
/// SIGNED intent `--to-store B`, B pulls → the message is committed and (because
/// the signature verified against A's registered key) attributed to A. Run with
/// `strict_verify` ON to prove the committed message was genuinely verified, not
/// merely advisory-accepted.
#[cfg(feature = "sign")]
#[test]
fn signed_cross_store_send_is_verified_then_committed() {
    let a = TestDb::new();
    let b = TestDb::new();
    let a_cfg = unique_config_home();
    let b_cfg = unique_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // A generates its signing keypair (private key 0600 under A's config dir).
    let keygen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let alice_pub = pubkey_from_gen(&keygen);

    // B registers alice's PUBLIC key so it can verify her signatures.
    run_ok_env(
        &b,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );

    // A sends a SIGNED cross-store intent for bob (signed with A's key file).
    run_ok_env(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "signed cross-store hello",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );

    // B pulls in STRICT mode: only a cryptographically-verified intent commits.
    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("WEAVE_STRICT_VERIFY", "1"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull.contains("pulled 1 message"),
        "a signature verified against the registered key commits even under strict: {pull}"
    );

    // The verified message is in B's inbox, attributed to alice.
    let inbox = run_ok_env(
        &b,
        &["inbox", "--me", "bob", "--json", "--peek"],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"][0]["body"], "signed cross-store hello");
    assert_eq!(
        v["messages"][0]["sender"], "alice",
        "the verified intent is attributed to the signed sender"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// `weave key gen` keeps the secret off stdout: only the PUBLIC key and the key
/// FILE PATH are printed, never the private key bytes.
#[cfg(feature = "sign")]
#[test]
fn key_gen_never_prints_the_private_key() {
    let a = TestDb::new();
    let a_cfg = unique_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();

    let keygen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );

    // Read the secret hex actually written to disk, and assert it is NOT in stdout.
    let key_file = a_cfg.join("weave").join("ed25519.key");
    let secret = std::fs::read_to_string(&key_file).expect("key file written");
    let secret = secret.trim();
    assert!(!secret.is_empty(), "key file holds the secret");
    assert!(
        !keygen.contains(secret),
        "the private key must never appear in `weave key gen` stdout"
    );
    // The public key, which IS printed, must differ from the secret bytes.
    let pubkey = pubkey_from_gen(&keygen);
    assert_ne!(pubkey, secret, "public key is not the secret");

    let _ = std::fs::remove_dir_all(&a_cfg);
}

// ---------------------------------------------------------------------------
// Tier-2 phase 2d (Feature #3) — TIGHTEN signed identity: trust-set strict-by-
// default, key rotation/overlap, revocation, and fingerprint listing. All
// `--features sign`, hermetic (no network), per-actor isolated XDG_CONFIG_HOME,
// fixed key files on disk. The decision-table semantics are unit-proven in
// `src/store.rs::verify_decision_table_every_cell`; these prove the SAME table
// holds end-to-end in the COMPILED binary through the CLI/MCP seams.
// ---------------------------------------------------------------------------

/// A distinct isolated config home for these Feature-#3 integration tests (separate
/// from `unique_config_home` to keep names unambiguous and counters independent).
#[cfg(feature = "sign")]
fn sign_config_home_it() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("weave-it-f3-{pid}-{n}-{nanos}"));
    std::fs::create_dir_all(&d).expect("create temp config home");
    d
}

/// The FULL-digest fingerprint (`SHA256:<64-hex>`) for `pubkey`, as the BINARY
/// itself derives it: `weave key revoke <pubkey>` normalizes a bare pubkey to its
/// `SHA256:<full-64-hex>` form on stdout. This is the ONLY value trust/revoke match
/// against (R3) — weave displays a truncated `SHA256:<16-hex>` for humans but emits
/// the full digest in every trust line. We derive it from the binary (never a
/// hand-rolled hash) so the test trusts exactly what production computes.
#[cfg(feature = "sign")]
fn full_fp_of_registered(db: &TestDb, cfg_home: &str, pubkey: &str) -> String {
    let out = run_ok_env(
        db,
        &["key", "revoke", pubkey],
        &[("XDG_CONFIG_HOME", cfg_home)],
    );
    out.lines()
        .find_map(|l| l.trim().strip_prefix("WEAVE_REVOKED="))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| panic!("`weave key revoke <pubkey>` did not echo a full fp:\n{out}"))
}

/// `weave key fingerprint` prints the local `SHA256:` display fingerprint and is
/// secret-free; `--json` carries identity + pubkey + fingerprint and no secret.
#[cfg(feature = "sign")]
#[test]
fn key_fingerprint_prints_secret_free_fp() {
    let a = TestDb::new();
    let a_cfg = sign_config_home_it();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();

    let keygen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let pubkey = pubkey_from_gen(&keygen);
    let secret =
        std::fs::read_to_string(a_cfg.join("weave").join("ed25519.key")).expect("key file");
    let secret = secret.trim();

    let fp_txt = run_ok_env(
        &a,
        &["key", "fingerprint", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    assert!(
        fp_txt.contains("SHA256:"),
        "fingerprint prints a SHA256: form: {fp_txt}"
    );
    assert!(
        !fp_txt.contains(secret),
        "the private key must never appear in `key fingerprint`"
    );

    let fp_json = run_ok_env(
        &a,
        &["key", "fingerprint", "--me", "alice", "--json"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let v: serde_json::Value = serde_json::from_str(&fp_json).expect("fingerprint --json parses");
    assert_eq!(v["identity"], "alice");
    assert_eq!(v["pubkey"], pubkey);
    assert!(
        v["fingerprint"].as_str().unwrap().starts_with("SHA256:"),
        "json fingerprint is SHA256-labeled: {fp_json}"
    );
    assert!(
        !fp_json.contains(secret),
        "the private key must never appear in `key fingerprint --json`"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
}

/// `weave key list --json` surfaces each registered key's fingerprint plus the
/// receiver-local trusted/revoked tags, and echoes the configured trust set —
/// secret-free.
#[cfg(feature = "sign")]
#[test]
fn key_list_json_shows_fingerprints_and_trust_tags() {
    let alice_store = TestDb::new();
    let b = TestDb::new();
    let alice_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let alice_cfg_s = alice_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let agen = run_ok_env(
        &alice_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &alice_cfg_s)],
    );
    let alice_pub = pubkey_from_gen(&agen);
    run_ok_env(
        &b,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let alice_full = full_fp_of_registered(&b, &b_cfg_s, &alice_pub);

    let list = run_ok_env(
        &b,
        &["key", "list", "--json"],
        &[("WEAVE_TRUST", &alice_full), ("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let v: serde_json::Value = serde_json::from_str(&list).expect("key list --json parses");
    let key0 = &v["keys"][0];
    assert_eq!(key0["identity"], "alice");
    assert!(
        key0["fingerprint"].as_str().unwrap().starts_with("SHA256:"),
        "listed key carries a SHA256: fingerprint: {list}"
    );
    assert_eq!(key0["trusted"], true, "alice is tagged trusted: {list}");
    assert_eq!(key0["revoked"], false, "alice not revoked: {list}");
    assert_eq!(v["trust_set"][0], alice_full, "trust_set echoed: {list}");

    let _ = std::fs::remove_dir_all(&alice_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// `weave key rotate` archives the OLD key (0600 .bak that exists and holds the old
/// secret, with a NEW distinct on-disk key), prints BOTH fingerprints and a
/// WEAVE_TRUST overlap line carrying both full fps, and never prints either secret.
#[cfg(feature = "sign")]
#[test]
fn key_rotate_archives_old_prints_both_fps_secret_free() {
    use std::os::unix::fs::PermissionsExt;
    let a = TestDb::new();
    let a_cfg = sign_config_home_it();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let key_file = a_cfg.join("weave").join("ed25519.key");

    let gen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let old_pub = pubkey_from_gen(&gen);
    let old_secret = std::fs::read_to_string(&key_file)
        .expect("old key")
        .trim()
        .to_string();

    let rot = run_ok_env(
        &a,
        &["key", "rotate", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    assert!(
        rot.contains("old fingerprint:") && rot.contains("new fingerprint:"),
        "rotate prints both old and new fingerprints: {rot}"
    );
    assert!(
        rot.contains(&old_pub),
        "rotate echoes the old pubkey: {rot}"
    );
    assert!(
        !rot.contains(&old_secret),
        "rotate must never print the OLD private key"
    );

    let new_secret = std::fs::read_to_string(&key_file)
        .expect("new key")
        .trim()
        .to_string();
    assert_ne!(old_secret, new_secret, "rotate writes a NEW distinct key");
    assert!(
        !rot.contains(&new_secret),
        "rotate must never print the NEW private key"
    );

    // The .bak archive of the OLD key exists, is 0600, and holds the OLD secret.
    let bak = std::fs::read_dir(a_cfg.join("weave"))
        .expect("config dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("ed25519.key.") && n.contains(".bak"))
                .unwrap_or(false)
        })
        .expect("rotate archives the old key to a .bak file");
    let mode = std::fs::metadata(&bak).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o177,
        0,
        "archived old key is 0600; mode {:o}",
        mode & 0o777
    );
    assert_eq!(
        std::fs::read_to_string(&bak).expect("read bak").trim(),
        old_secret,
        "the archive holds the OLD secret (recoverable for overlap)"
    );

    // The overlap guidance carries BOTH full fps in a WEAVE_TRUST line.
    let trust_line = rot
        .lines()
        .find_map(|l| l.trim().strip_prefix("WEAVE_TRUST="))
        .unwrap_or("");
    assert!(
        trust_line.contains(',') && trust_line.contains("SHA256:"),
        "rotate emits a WEAVE_TRUST line with BOTH full fps for overlap: {rot}"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
}

/// DEFAULT-WHEN-TRUST-SET (the headline tightening): with `WEAVE_TRUST=<alice-fp>`
/// and NO `WEAVE_STRICT_VERIFY`, an UNSIGNED intent claiming alice is REJECTED
/// (pulled 0), while a SIGNED intent from alice COMMITS — strict-by-default for a
/// trusted sender, end-to-end through the binary.
#[cfg(feature = "sign")]
#[test]
fn trust_set_makes_unsigned_trusted_sender_rejected_signed_commits() {
    let a = TestDb::new();
    let unsigned_src = TestDb::new();
    let b_signed = TestDb::new();
    let b_unsigned = TestDb::new();
    let a_cfg = sign_config_home_it();
    let nokey_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let nokey_cfg_s = nokey_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let agen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let alice_pub = pubkey_from_gen(&agen);
    run_ok_env(
        &b_signed,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &b_unsigned,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let alice_full = full_fp_of_registered(&b_signed, &b_cfg_s, &alice_pub);

    // (1) UNSIGNED intent CLAIMING alice ⇒ trust set rejects (no strict flag set).
    run_ok_env(
        &unsigned_src,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "unsigned-claiming-alice",
            "--to-store",
            &b_unsigned.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &nokey_cfg_s)],
    );
    let pull_u = run_ok_env(
        &b_unsigned,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &unsigned_src.path_str()),
            ("WEAVE_TRUST", &alice_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull_u.contains("pulled 0 message"),
        "a trusted sender's UNSIGNED message is rejected by the trust set alone (no strict flag): {pull_u}"
    );

    // (2) SIGNED intent from alice ⇒ commits under the same trust set.
    run_ok_env(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "signed-from-alice",
            "--to-store",
            &b_signed.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let pull_s = run_ok_env(
        &b_signed,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("WEAVE_TRUST", &alice_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull_s.contains("pulled 1 message"),
        "a trusted sender's SIGNED message commits under the trust set: {pull_s}"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&nokey_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// NO-TRUST-SET REGRESSION: with no `WEAVE_TRUST`, an UNSIGNED intent still COMMITS
/// (advisory) — unsigned operation preserved for users who never opt in.
#[cfg(feature = "sign")]
#[test]
fn no_trust_set_unsigned_still_commits() {
    let src = TestDb::new();
    let b = TestDb::new();
    let src_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let src_cfg_s = src_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    run_ok_env(
        &src,
        &[
            "send",
            "--from",
            "dave",
            "--to",
            "bob",
            "--body",
            "unsigned-no-trust",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &src_cfg_s)],
    );
    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &src.path_str()),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull.contains("pulled 1 message"),
        "with NO trust set, an unsigned intent still commits (advisory preserved): {pull}"
    );

    let _ = std::fs::remove_dir_all(&src_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// ROTATION OVERLAP end-to-end (R6, config-based, no schema): a receiver trusting
/// BOTH old+new fps commits a message signed by the OLD key while the OLD pubkey is
/// registered; once the OLD fp is `WEAVE_REVOKED`, the OLD key's signed message is
/// REJECTED while a NEW-key signed message (new pubkey registered) still commits.
#[cfg(feature = "sign")]
#[test]
fn rotation_overlap_then_revoke_old_through_binary() {
    let old_store = TestDb::new();
    let new_store = TestDb::new();
    let old_cfg = sign_config_home_it();
    let new_cfg = sign_config_home_it();
    let old_cfg_s = old_cfg.to_string_lossy().into_owned();
    let new_cfg_s = new_cfg.to_string_lossy().into_owned();

    let ogen = run_ok_env(
        &old_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    );
    let old_pub = pubkey_from_gen(&ogen);
    let ngen = run_ok_env(
        &new_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    );
    let new_pub = pubkey_from_gen(&ngen);

    let fp_helper = TestDb::new();
    let fph_cfg = sign_config_home_it();
    let fph_cfg_s = fph_cfg.to_string_lossy().into_owned();
    let old_full = full_fp_of_registered(&fp_helper, &fph_cfg_s, &old_pub);
    let new_full = full_fp_of_registered(&fp_helper, &fph_cfg_s, &new_pub);
    let both_trust = format!("{old_full},{new_full}");

    let b_cfg = sign_config_home_it();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // OVERLAP: B registers OLD pubkey, trusts BOTH ⇒ old-key signed commits.
    let b_overlap = TestDb::new();
    run_ok_env(
        &b_overlap,
        &["key", "add", "alice", &old_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &old_store,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "old-key-overlap",
            "--to-store",
            &b_overlap.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    );
    let p_overlap = run_ok_env(
        &b_overlap,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &old_store.path_str()),
            ("WEAVE_TRUST", &both_trust),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_overlap.contains("pulled 1 message"),
        "old key still verifies during overlap (old pubkey registered, both fps trusted): {p_overlap}"
    );

    // AFTER REVOKE: B registers OLD pubkey, revokes OLD fp ⇒ old-key signed REJECTED.
    let b_revold = TestDb::new();
    run_ok_env(
        &b_revold,
        &["key", "add", "alice", &old_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &old_store,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "old-key-after-revoke",
            "--to-store",
            &b_revold.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    );
    let p_revold = run_ok_env(
        &b_revold,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &old_store.path_str()),
            ("WEAVE_TRUST", &new_full),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_revold.contains("pulled 0 message"),
        "old key's signed message is REJECTED once its fp is revoked: {p_revold}"
    );

    // NEW key still commits after the old fp is revoked.
    let b_new = TestDb::new();
    run_ok_env(
        &b_new,
        &["key", "add", "alice", &new_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &new_store,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "new-key-after-revoke",
            "--to-store",
            &b_new.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    );
    let p_new = run_ok_env(
        &b_new,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &new_store.path_str()),
            ("WEAVE_TRUST", &new_full),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_new.contains("pulled 1 message"),
        "the new key still commits after the old fp is revoked: {p_new}"
    );

    let _ = std::fs::remove_dir_all(&old_cfg);
    let _ = std::fs::remove_dir_all(&new_cfg);
    let _ = std::fs::remove_dir_all(&fph_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// MCP drain path under a configured trust set DROPS a trusted-but-unsigned pulled
/// intent (store-only, `isError:false`, never a panic) and leaks no secret.
#[cfg(feature = "sign")]
#[test]
fn mcp_drain_under_trust_set_drops_unsigned_trusted_sender() {
    let a = TestDb::new();
    let unsigned_src = TestDb::new();
    let b = TestDb::new();
    let a_cfg = sign_config_home_it();
    let nokey_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let nokey_cfg_s = nokey_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let agen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let alice_pub = pubkey_from_gen(&agen);
    run_ok_env(
        &b,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let alice_full = full_fp_of_registered(&b, &b_cfg_s, &alice_pub);

    run_ok_env(
        &unsigned_src,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "unsigned-via-mcp-drain",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &nokey_cfg_s)],
    );

    let mut mcp = McpServer::spawn_env(
        &b,
        &[
            ("WEAVE_PULL_FROM", &unsigned_src.path_str()),
            ("WEAVE_TRUST", &alice_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    let (err, text) = mcp.call_tool("weave_inbox", serde_json::json!({"me": "bob"}));
    assert!(
        !err,
        "weave_inbox under a trust set is not an error (store-only drop): {text}"
    );
    assert!(
        !text.contains("unsigned-via-mcp-drain"),
        "the trusted-but-unsigned intent must be DROPPED on the drain, not delivered: {text}"
    );

    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "weave_doctor is not an error: {dtext}");
    assert!(
        !dtext.contains("private key"),
        "doctor never prints a private key: {dtext}"
    );
    mcp.shutdown();

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&nokey_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

// ---------------------------------------------------------------------------
// Tier-2 v2 — remote (libsql/Turso) source handling, hermetic (NO network).
//
// On the DEFAULT sqlite build a remote URL is rejected LOUDLY at the store seam
// ("requires --features libsql"); local sources still work and the command
// succeeds. On the libsql build the remote is actually attempted and (with an
// unreachable host + a tiny timeout) skipped as a per-source failure. Either way
// the command succeeds for any local source and the URL/token never reach stdout.
// ---------------------------------------------------------------------------

/// `WEAVE_PEER_DBS` mixing a LOCAL store and a REMOTE URL on the default build:
/// the local peer is listed, the command succeeds, and the remote is skipped with
/// the loud "--features libsql" note on stderr (never stdout). The token never
/// appears anywhere in the output.
#[test]
fn federation_mixed_local_and_remote_source() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_ok(&foreign, &["register", "--name", "fedpeer"]);
    run_ok(&local, &["register", "--name", "localpeer"]);

    const TOKEN: &str = "super-secret-pull-token-value";
    let remote_url = "libsql://unreachable-host.invalid";
    let list = format!("{},{remote_url}", foreign.path_str());

    let (ok, out, err) = run_env(
        &local,
        &["peers"],
        &[
            ("WEAVE_PEER_DBS", &list),
            ("WEAVE_PULL_TOKEN", TOKEN),
            // Keep the libsql-build connect attempt fast (no network wait).
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    assert!(ok, "command must succeed for local sources; stderr:\n{err}");
    assert!(out.contains("localpeer"), "local peer listed: {out}");
    assert!(out.contains("fedpeer"), "local foreign peer listed: {out}");

    // The token is NEVER printed (stdout or stderr).
    assert!(!out.contains(TOKEN), "token leaked to stdout: {out}");
    assert!(!err.contains(TOKEN), "token leaked to stderr: {err}");

    if cfg!(feature = "libsql") {
        // The libsql build actually attempts the (unreachable) remote and skips it
        // as a federated-store failure on stderr — never fatal.
        assert!(
            err.contains("skipping federated store") || err.contains("unreachable-host"),
            "libsql build should diagnose the unreachable remote on stderr: {err}"
        );
    } else {
        // The default sqlite build rejects the remote loudly with the rebuild note.
        assert!(
            err.contains("--features libsql"),
            "default build must tell the user to rebuild for remote: {err}"
        );
        assert!(
            !out.contains("--features libsql"),
            "the rebuild note must go to stderr, never stdout: {out}"
        );
    }
    // The redacted scheme+host may appear in the skip note, but never the path/token.
    assert!(!out.contains("libsql://"), "no remote URL on stdout: {out}");
}

/// `weave doctor --json` reports the configured remote-source count, and on the
/// default sqlite build the `federation_remote_unsupported` count is non-zero
/// (the user is told the remote was skipped for lack of the feature). The token
/// never appears in the doctor output.
#[test]
fn doctor_reports_remote_source_count() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const TOKEN: &str = "doctor-secret-token";
    let remote_url = "libsql://reporting-host.invalid";

    let (ok, out, err) = run_env(
        &local,
        &["doctor", "--json"],
        &[
            ("WEAVE_PEER_DBS", remote_url),
            ("WEAVE_PULL_TOKEN", TOKEN),
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    assert!(ok, "doctor must succeed; stderr:\n{err}");
    assert!(!out.contains(TOKEN), "token leaked to doctor stdout: {out}");
    assert!(!err.contains(TOKEN), "token leaked to doctor stderr: {err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_stores"].as_u64(),
        Some(1),
        "one remote source configured: {out}"
    );
    if !cfg!(feature = "libsql") {
        assert_eq!(
            v["federation_remote_unsupported"].as_u64(),
            Some(1),
            "default build must report the remote as unsupported: {out}"
        );
    }
}

/// MCP degradation: `weave_peers` with a remote source on the default build is a
/// SUCCESSFUL tool result (degradation is never a tool error), the local peer is
/// present, and the token never appears in the tool output.
#[test]
fn mcp_peers_with_remote_source_degrades_cleanly() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "localpeer"]);

    const TOKEN: &str = "mcp-secret-token";
    let remote_url = "libsql://mcp-host.invalid";

    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PEER_DBS", remote_url),
            ("WEAVE_PULL_TOKEN", TOKEN),
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    let (perr, ptext) = mcp.call_tool("weave_peers", serde_json::json!({}));
    assert!(!perr, "remote degradation is not a tool error: {ptext}");
    assert!(ptext.contains("localpeer"), "local peer present: {ptext}");
    assert!(
        !ptext.contains(TOKEN),
        "token leaked into MCP output: {ptext}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Tier-2 v2 follow-up — per-source pull tokens (WEAVE_PULL_TOKEN_<LABEL>),
// hermetic (NO network). These assert RESOLUTION + token-tier OBSERVABILITY and
// the headline secret-hygiene invariant (no token byte ever reaches stdout or
// stderr), NOT live remote auth. A labelled remote (`MYDB=libsql://host/db`) +
// a per-source `WEAVE_PULL_TOKEN_MYDB` resolves the per-source tier; without the
// per-source env it falls through to the shared `WEAVE_PULL_TOKEN`; with neither
// it is tier=none. The remote host is `.invalid` with a 200ms timeout so the
// libsql build never actually waits on a network.
// ---------------------------------------------------------------------------

/// A labelled remote in `WEAVE_PEER_DBS` plus its per-source `WEAVE_PULL_TOKEN_<LABEL>`
/// resolves the PER-SOURCE token tier in `doctor --json`. The secret token never
/// appears in stdout or stderr on EITHER backend, and on the default sqlite build
/// the remote is still loudly rejected (rebuild note on stderr, never stdout).
#[test]
fn federation_per_source_token_selected() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const PER_SOURCE: &str = "per-source-secret-token-AAA";
    const SHARED: &str = "shared-secret-token-BBB";
    let remote_url = "libsql://prod-host.invalid/db";
    let entry = format!("MYDB={remote_url}");

    let (ok, out, err) = run_env(
        &local,
        &["doctor", "--json"],
        &[
            ("WEAVE_PEER_DBS", &entry),
            ("WEAVE_PULL_TOKEN_MYDB", PER_SOURCE),
            ("WEAVE_PULL_TOKEN", SHARED),
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    assert!(ok, "doctor must succeed; stderr:\n{err}");

    // Headline secret-hygiene: NEITHER token appears anywhere.
    assert!(
        !out.contains(PER_SOURCE),
        "per-source token in stdout: {out}"
    );
    assert!(
        !err.contains(PER_SOURCE),
        "per-source token in stderr: {err}"
    );
    assert!(!out.contains(SHARED), "shared token in stdout: {out}");
    assert!(!err.contains(SHARED), "shared token in stderr: {err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(1),
        "labelled remote with its env token resolves the per-source tier: {out}"
    );
    assert_eq!(
        v["federation_remote_token_shared"].as_u64(),
        Some(0),
        "no shared-tier remote in this config: {out}"
    );

    if !cfg!(feature = "libsql") {
        // The remote is still rejected loudly on the default build (token-free).
        assert_eq!(
            v["federation_remote_unsupported"].as_u64(),
            Some(1),
            "default build reports the remote as unsupported: {out}"
        );
        assert!(
            err.contains("--features libsql"),
            "default build tells the user to rebuild for remote: {err}"
        );
    }
}

/// A labelled remote with NO `WEAVE_PULL_TOKEN_<LABEL>` set but with the shared
/// `WEAVE_PULL_TOKEN` set falls through to the SHARED token tier. The shared token
/// never appears in the output.
#[test]
fn federation_label_falls_through_to_shared() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const SHARED: &str = "shared-only-secret-CCC";
    let remote_url = "libsql://stage-host.invalid/db";
    let entry = format!("STAGE={remote_url}");

    // Note: WEAVE_PULL_TOKEN_STAGE deliberately UNSET (not in extra_env).
    let (ok, out, err) = run_env(
        &local,
        &["doctor", "--json"],
        &[
            ("WEAVE_PEER_DBS", &entry),
            ("WEAVE_PULL_TOKEN", SHARED),
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    assert!(ok, "doctor must succeed; stderr:\n{err}");
    assert!(!out.contains(SHARED), "shared token in stdout: {out}");
    assert!(!err.contains(SHARED), "shared token in stderr: {err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_token_shared"].as_u64(),
        Some(1),
        "labelled remote with no per-source env falls through to shared: {out}"
    );
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(0),
        "no per-source-tier remote in this config: {out}"
    );
}

/// A labelled remote with NEITHER a per-source env token NOR a shared token is
/// tier=none. The command still succeeds (no panic) and nothing is printed.
#[test]
fn federation_no_token_tier_none() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    let remote_url = "libsql://lonely-host.invalid/db";
    let entry = format!("SOLO={remote_url}");

    let (ok, out, err) = run_env(
        &local,
        &["doctor", "--json"],
        &[("WEAVE_PEER_DBS", &entry), ("WEAVE_PULL_TIMEOUT_MS", "200")],
    );
    assert!(ok, "doctor must succeed; stderr:\n{err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_token_none"].as_u64(),
        Some(1),
        "labelled remote with no token resolves tier=none: {out}"
    );
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(0),
        "no per-source token configured: {out}"
    );
    assert_eq!(
        v["federation_remote_token_shared"].as_u64(),
        Some(0),
        "no shared token configured: {out}"
    );
}

/// A LOCAL path that contains `=` (`a=b.db`) must NOT be misparsed as a `LABEL=`
/// split: it stays a verbatim local source (which fails to open as a weave store
/// and is skipped), and is never treated as a labelled remote. The command still
/// succeeds on a valid local source. Guards the degradation contract end-to-end.
#[test]
fn federation_local_path_with_equals_not_misparsed() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_ok(&foreign, &["register", "--name", "fedpeer"]);
    run_ok(&local, &["register", "--name", "here"]);

    // A real local foreign store + a junk `a=b.db` local entry (right side not a URL).
    let list = format!("{},a=b.db", foreign.path_str());
    let (ok, out, err) = run_env(&local, &["doctor", "--json"], &[("WEAVE_PEER_DBS", &list)]);
    assert!(ok, "doctor must succeed; stderr:\n{err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    // `a=b.db` is a LOCAL entry, never a remote — so the remote count excludes it.
    assert_eq!(
        v["federation_remote_stores"].as_u64(),
        Some(0),
        "a local `a=b.db` must not be counted as a remote: {out}"
    );
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(0),
        "no per-source remote tier from a local path: {out}"
    );
}

/// MCP `weave_doctor` reports the per-source token tier consistently with the CLI,
/// is a SUCCESSFUL tool result, and never leaks the token into the tool output.
#[test]
fn mcp_doctor_per_source_token_tier_is_token_free() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "localpeer"]);

    const PER_SOURCE: &str = "mcp-per-source-secret-DDD";
    let remote_url = "libsql://mcp-prod.invalid/db";
    let entry = format!("MCPDB={remote_url}");

    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PEER_DBS", &entry),
            ("WEAVE_PULL_TOKEN_MCPDB", PER_SOURCE),
            ("WEAVE_PULL_TIMEOUT_MS", "200"),
        ],
    );
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "doctor degradation is not a tool error: {dtext}");
    assert!(
        !dtext.contains(PER_SOURCE),
        "per-source token leaked into MCP output: {dtext}"
    );
    assert!(
        dtext.contains("remote tokens:"),
        "MCP doctor surfaces the token-tier line: {dtext}"
    );
    assert!(
        dtext.contains("1 per-source"),
        "MCP doctor reports the per-source tier count: {dtext}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Feature #2 — per-source remote-call TIMEOUT (WEAVE_PULL_TIMEOUT_MS[_<LABEL>]),
// surfaced in `weave doctor` (CLI + MCP). Hermetic (NO network): `.invalid`
// hosts + a short global timeout so the libsql build never waits on a real
// connect. These assert RESOLUTION + the doctor timeout-tier observability and
// the secret-hygiene invariant (no token byte ever reaches stdout/stderr); the
// resolution is backend-agnostic, so the COUNTS hold on BOTH builds.
// ---------------------------------------------------------------------------

/// `weave doctor --json` reports per-source timeout-tier counts: a labelled
/// remote with `WEAVE_PULL_TIMEOUT_MS_<LABEL>` set resolves the per-source tier,
/// while a sibling unlabelled remote with only the global `WEAVE_PULL_TIMEOUT_MS`
/// resolves the global tier. The effective ms min/max stay within the clamp
/// bounds `[50, 600000]`. A second human-form doctor surfaces the `remote
/// timeout:` line. Counts hold on BOTH backends (resolution is backend-agnostic).
#[test]
fn doctor_reports_per_source_timeout() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    // PROD is labelled (per-source 250ms); the second entry is unlabelled and
    // falls back to the global 1000ms. Both `.invalid` so libsql never connects.
    let peer_dbs = "PROD=libsql://h.invalid,libsql://g.invalid";

    let (ok, out, err) = run_env(
        &local,
        &["doctor", "--json"],
        &[
            ("WEAVE_PEER_DBS", peer_dbs),
            ("WEAVE_PULL_TIMEOUT_MS_PROD", "250"),
            ("WEAVE_PULL_TIMEOUT_MS", "1000"),
        ],
    );
    assert!(ok, "doctor must succeed; stderr:\n{err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_stores"].as_u64(),
        Some(2),
        "two remote sources configured: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_per_source"].as_u64(),
        Some(1),
        "the labelled PROD remote resolves the per-source timeout tier: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_global"].as_u64(),
        Some(1),
        "the unlabelled remote falls back to the global timeout tier: {out}"
    );
    let min = v["federation_remote_timeout_ms_min"]
        .as_u64()
        .expect("ms_min present when remotes configured");
    let max = v["federation_remote_timeout_ms_max"]
        .as_u64()
        .expect("ms_max present when remotes configured");
    assert!(
        (50..=600_000).contains(&min) && (50..=600_000).contains(&max),
        "effective ms within clamp bounds: min={min} max={max} ({out})"
    );
    assert_eq!(min, 250, "min effective ms is the per-source 250: {out}");
    assert_eq!(max, 1000, "max effective ms is the global 1000: {out}");

    // The human form surfaces the per-source timeout line (token-free).
    let (ok2, out2, err2) = run_env(
        &local,
        &["doctor"],
        &[
            ("WEAVE_PEER_DBS", peer_dbs),
            ("WEAVE_PULL_TIMEOUT_MS_PROD", "250"),
            ("WEAVE_PULL_TIMEOUT_MS", "1000"),
        ],
    );
    assert!(ok2, "human doctor must succeed; stderr:\n{err2}");
    assert!(
        out2.contains("remote timeout:"),
        "human doctor surfaces the per-source timeout line: {out2}"
    );

    if cfg!(feature = "libsql") {
        // The libsql build actually attempts the (unreachable) remotes and
        // diagnoses them as skipped federated stores on stderr (never fatal,
        // never stdout). The short per-source/global timeouts bound the wait.
        assert!(
            err.contains("skipping federated store")
                || err.contains("h.invalid")
                || err.contains("g.invalid"),
            "libsql build diagnoses the unreachable remotes on stderr: {err}"
        );
    }
}

/// A labelled remote with NO timeout env set resolves the DEFAULT tier, and the
/// doctor reports `REMOTE_TIMEOUT_MS_DEFAULT` (5000) as the effective ms. Holds
/// on both backends (resolution is backend-agnostic).
#[test]
fn doctor_timeout_falls_back_to_default() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    // Labelled remote, but NO WEAVE_PULL_TIMEOUT_MS[_LABEL] in the scrubbed env.
    let entry = "NOENV=libsql://noenv.invalid/db";

    let (ok, out, err) = run_env(&local, &["doctor", "--json"], &[("WEAVE_PEER_DBS", entry)]);
    assert!(ok, "doctor must succeed; stderr:\n{err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_timeout_default"].as_u64(),
        Some(1),
        "the unconfigured remote resolves the default timeout tier: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_per_source"].as_u64(),
        Some(0),
        "no per-source timeout configured: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_global"].as_u64(),
        Some(0),
        "no global timeout configured: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_ms_min"].as_u64(),
        Some(5000),
        "default-tier effective ms is REMOTE_TIMEOUT_MS_DEFAULT: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_ms_max"].as_u64(),
        Some(5000),
        "default-tier effective ms is REMOTE_TIMEOUT_MS_DEFAULT: {out}"
    );
}

/// MCP `weave_doctor` mirrors the per-source timeout line of the CLI: with a
/// labelled remote + per-source timeout env, the tool RESULT contains `remote
/// timeout:` and NONE of the configured token bytes leak. Successful tool result.
#[test]
fn mcp_doctor_reports_per_source_timeout_token_free() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "localpeer"]);

    const PER_SOURCE_TOKEN: &str = "mcp-timeout-secret-token-EEE";
    let entry = "TODB=libsql://mcp-timeout.invalid/db";

    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PEER_DBS", entry),
            ("WEAVE_PULL_TIMEOUT_MS_TODB", "250"),
            ("WEAVE_PULL_TIMEOUT_MS", "1000"),
            ("WEAVE_PULL_TOKEN_TODB", PER_SOURCE_TOKEN),
        ],
    );
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "doctor degradation is not a tool error: {dtext}");
    assert!(
        dtext.contains("remote timeout:"),
        "MCP doctor surfaces the per-source timeout line: {dtext}"
    );
    assert!(
        dtext.contains("1 per-source"),
        "MCP doctor reports the per-source timeout tier count: {dtext}"
    );
    assert!(
        !dtext.contains(PER_SOURCE_TOKEN),
        "no token byte leaks into the MCP doctor result: {dtext}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Feature #9 — federation token/timeout PARITY: the `pull_from` (Tier-2
// delivery) side is now SURFACED in `doctor` at parity with `peer_db`. These
// drive the compiled binary with `.invalid` hosts (NO live network) and assert
// the NEW additive `federation_pull_*` keys + the human "pull sources/tokens/
// timeout" block, that ALL existing `federation_*` (peer_db) keys are
// unchanged, that a local-only/unconfigured config emits NO pull block
// (backward-compat), and the headline: no token byte ever reaches stdout/stderr
// across the new pull surface. Resolution is backend-agnostic, so counts hold
// on BOTH the default sqlite and the `--features libsql` builds.
// ---------------------------------------------------------------------------

/// A MIX of federation sources: `WEAVE_PULL_FROM` carries a labelled remote
/// (`.invalid`, with its own per-source token + per-source timeout) plus a local
/// path, AND `WEAVE_PEER_DBS` carries a second labelled remote with its own
/// token + timeout. `weave doctor --json` must surface the NEW
/// `federation_pull_*` keys (the previously-MISSING pull token tier now
/// reported), keep the existing peer_db `federation_remote_*` keys correct, and
/// keep the effective ms range within the clamp bounds. NEITHER token byte ever
/// appears on stdout/stderr. Holds on both backends.
#[test]
fn doctor_surfaces_pull_from_federation_health() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);
    // A DISTINCT foreign local store (the resolver canonical-dedups any source
    // equal to the caller's own db_path, so it must NOT be `local`).
    let foreign_local = TestDb::new();
    run_ok(&foreign_local, &["register", "--name", "fedlocal"]);

    const PULL_TOK: &str = "pull-side-per-source-token-FFF";
    const PEER_TOK: &str = "peer-side-per-source-token-GGG";

    // pull_from: one labelled remote (per-source token 250ms) + one local.
    let pull_from = format!(
        "PULLP=libsql://pull-prod.invalid,{}",
        foreign_local.path_str()
    );
    // peer_db: one labelled remote (per-source token, global-timeout fallback).
    let peer_dbs = "PEERP=libsql://peer-prod.invalid";

    let env: &[(&str, &str)] = &[
        ("WEAVE_PULL_FROM", &pull_from),
        ("WEAVE_PULL_TOKEN_PULLP", PULL_TOK),
        ("WEAVE_PULL_TIMEOUT_MS_PULLP", "250"),
        ("WEAVE_PEER_DBS", peer_dbs),
        ("WEAVE_PULL_TOKEN_PEERP", PEER_TOK),
        ("WEAVE_PULL_TIMEOUT_MS", "1000"),
    ];

    let (ok, out, err) = run_env(&local, &["doctor", "--json"], env);
    assert!(ok, "doctor --json must succeed; stderr:\n{err}");

    // Headline secret-hygiene: neither token appears anywhere.
    assert!(!out.contains(PULL_TOK), "pull token in json stdout: {out}");
    assert!(!err.contains(PULL_TOK), "pull token in json stderr: {err}");
    assert!(!out.contains(PEER_TOK), "peer token in json stdout: {out}");
    assert!(!err.contains(PEER_TOK), "peer token in json stderr: {err}");

    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");

    // NEW pull-side keys: 2 sources (1 local + 1 remote); the remote resolves the
    // per-source token tier + the per-source timeout tier (the formerly-MISSING
    // pull token surface is now reported).
    assert_eq!(
        v["federation_pull_sources"].as_u64(),
        Some(2),
        "pull_from: one local + one remote = 2 sources: {out}"
    );
    assert_eq!(v["federation_pull_local"].as_u64(), Some(1), "{out}");
    assert_eq!(v["federation_pull_remote"].as_u64(), Some(1), "{out}");
    assert_eq!(
        v["federation_pull_token_per_source"].as_u64(),
        Some(1),
        "pull remote resolves its OWN per-source token tier: {out}"
    );
    assert_eq!(v["federation_pull_token_shared"].as_u64(), Some(0), "{out}");
    assert_eq!(v["federation_pull_token_none"].as_u64(), Some(0), "{out}");
    assert_eq!(
        v["federation_pull_timeout_per_source"].as_u64(),
        Some(1),
        "pull remote resolves the per-source 250ms timeout tier: {out}"
    );
    let pmin = v["federation_pull_timeout_ms_min"]
        .as_u64()
        .expect("pull ms_min present when a remote pull source exists");
    let pmax = v["federation_pull_timeout_ms_max"]
        .as_u64()
        .expect("pull ms_max present when a remote pull source exists");
    assert_eq!(
        pmin, 250,
        "pull effective min ms is the per-source 250: {out}"
    );
    assert_eq!(pmax, 250, "single pull remote ⇒ min==max==250: {out}");
    assert!(
        (50..=600_000).contains(&pmin) && (50..=600_000).contains(&pmax),
        "pull ms within clamp bounds: {out}"
    );

    // EXISTING peer_db keys remain correct (additive, no regression): one remote
    // peer_db source resolving its OWN per-source token + the global timeout tier.
    assert_eq!(
        v["federation_remote_stores"].as_u64(),
        Some(1),
        "one peer_db remote configured: {out}"
    );
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(1),
        "peer_db remote resolves its per-source token tier: {out}"
    );
    assert_eq!(
        v["federation_remote_timeout_global"].as_u64(),
        Some(1),
        "peer_db remote falls back to the global 1000ms timeout: {out}"
    );

    // The human form prints the additive pull block (token-free).
    let (ok2, out2, err2) = run_env(&local, &["doctor"], env);
    assert!(ok2, "human doctor must succeed; stderr:\n{err2}");
    assert!(
        !out2.contains(PULL_TOK),
        "pull token in human stdout: {out2}"
    );
    assert!(
        !out2.contains(PEER_TOK),
        "peer token in human stdout: {out2}"
    );
    assert!(
        out2.contains("pull sources:"),
        "human doctor surfaces the pull-source line: {out2}"
    );
    assert!(
        out2.contains("pull tokens:") && out2.contains("pull timeout:"),
        "human doctor surfaces the pull token + timeout lines: {out2}"
    );
}

/// Backward-compat: with NO federation configured (no `WEAVE_PULL_FROM`, no
/// `WEAVE_PEER_DBS`), `doctor --json` carries NONE of the new `federation_pull_*`
/// keys and the human form prints NO pull block — so existing local-only output
/// is byte-unchanged. A local-only `WEAVE_PULL_FROM` reports a count but emits no
/// remote tier/timeout keys (no misleading 0-0).
#[test]
fn doctor_pull_block_absent_when_no_federation() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);
    // A distinct foreign local store for the local-only case (the resolver
    // canonical-dedups any source equal to the caller's own db_path).
    let foreign_local = TestDb::new();
    run_ok(&foreign_local, &["register", "--name", "fedlocal"]);

    // (1) Nothing configured: pull block absent entirely.
    let (ok, out, err) = run_env(&local, &["doctor", "--json"], &[]);
    assert!(ok, "doctor --json must succeed; stderr:\n{err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    assert!(
        v.get("federation_pull_sources").is_none(),
        "no pull block when unconfigured: {out}"
    );
    assert!(
        v.get("federation_pull_token_per_source").is_none(),
        "no pull token keys when unconfigured: {out}"
    );

    let (okh, outh, errh) = run_env(&local, &["doctor"], &[]);
    assert!(okh, "human doctor must succeed; stderr:\n{errh}");
    assert!(
        !outh.contains("pull sources:"),
        "no pull block in human form when unconfigured: {outh}"
    );

    // (2) Local-only pull_from: a count surfaces but NO remote tier/ms keys.
    let (ok2, out2, err2) = run_env(
        &local,
        &["doctor", "--json"],
        &[("WEAVE_PULL_FROM", &foreign_local.path_str())],
    );
    assert!(ok2, "doctor --json must succeed; stderr:\n{err2}");
    let v2: serde_json::Value = serde_json::from_str(&out2).expect("doctor --json parses");
    assert_eq!(
        v2["federation_pull_sources"].as_u64(),
        Some(1),
        "local-only pull_from reports a source count: {out2}"
    );
    assert_eq!(v2["federation_pull_local"].as_u64(), Some(1), "{out2}");
    assert_eq!(v2["federation_pull_remote"].as_u64(), Some(0), "{out2}");
    assert!(
        v2.get("federation_pull_timeout_ms_min").is_none(),
        "no ms range over zero remote pull sources: {out2}"
    );
}

/// MCP `weave_doctor` mirrors the CLI pull-side block: with a labelled remote in
/// `WEAVE_PULL_FROM` + its per-source token, the tool RESULT contains the `pull
/// sources:`/`pull tokens:` lines and NO token byte leaks. Successful tool result.
#[test]
fn mcp_doctor_surfaces_pull_from_block_token_free() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "localpeer"]);

    const PULL_TOK: &str = "mcp-pull-per-source-token-HHH";
    let pull_from = "MCPPULL=libsql://mcp-pull.invalid/db";

    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PULL_FROM", pull_from),
            ("WEAVE_PULL_TOKEN_MCPPULL", PULL_TOK),
            ("WEAVE_PULL_TIMEOUT_MS_MCPPULL", "250"),
        ],
    );
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "doctor degradation is not a tool error: {dtext}");
    assert!(
        !dtext.contains(PULL_TOK),
        "no pull token byte leaks into the MCP doctor result: {dtext}"
    );
    assert!(
        dtext.contains("pull sources:"),
        "MCP doctor surfaces the pull-source line: {dtext}"
    );
    assert!(
        dtext.contains("pull tokens:") && dtext.contains("1 per-source"),
        "MCP doctor surfaces the pull token tier line: {dtext}"
    );
    mcp.shutdown();
}

/// Item-2 confirmation (headline): a per-source `WEAVE_PULL_TOKEN_<LABEL>` is
/// selected for a `WEAVE_PEER_DBS` (peer_db) remote — proving the LABEL namespace
/// covers peer_db, not just `pull_from`. Run `weave peers` AND `weave doctor
/// --json`: both succeed; NEITHER the per-source NOR the shared token byte appears
/// on stdout/stderr; doctor reports `federation_remote_token_per_source==1`. No
/// live Turso auth is asserted (`.invalid` host, short timeout).
#[test]
fn federation_peer_db_per_source_token_selected() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const TOK_A: &str = "peerdb-per-source-token-AAA";
    const SHARED: &str = "peerdb-shared-token-BBB";
    let entry = "PROD=libsql://prod.invalid";

    let env: &[(&str, &str)] = &[
        ("WEAVE_PEER_DBS", entry),
        ("WEAVE_PULL_TOKEN_PROD", TOK_A),
        ("WEAVE_PULL_TOKEN", SHARED),
        ("WEAVE_PULL_TIMEOUT_MS", "200"),
    ];

    // `weave peers` resolves + (on libsql) applies the per-source token; it must
    // never surface a token byte regardless of backend.
    let (ok_p, out_p, err_p) = run_env(&local, &["peers"], env);
    assert!(ok_p, "peers must succeed; stderr:\n{err_p}");
    assert!(
        !out_p.contains(TOK_A),
        "per-source token in peers stdout: {out_p}"
    );
    assert!(
        !err_p.contains(TOK_A),
        "per-source token in peers stderr: {err_p}"
    );
    assert!(
        !out_p.contains(SHARED),
        "shared token in peers stdout: {out_p}"
    );
    assert!(
        !err_p.contains(SHARED),
        "shared token in peers stderr: {err_p}"
    );

    // `weave doctor --json` proves the peer_db remote resolved its OWN token tier.
    let (ok_d, out_d, err_d) = run_env(&local, &["doctor", "--json"], env);
    assert!(ok_d, "doctor must succeed; stderr:\n{err_d}");
    assert!(
        !out_d.contains(TOK_A),
        "per-source token in doctor stdout: {out_d}"
    );
    assert!(
        !err_d.contains(TOK_A),
        "per-source token in doctor stderr: {err_d}"
    );
    assert!(
        !out_d.contains(SHARED),
        "shared token in doctor stdout: {out_d}"
    );
    assert!(
        !err_d.contains(SHARED),
        "shared token in doctor stderr: {err_d}"
    );

    let v: serde_json::Value = serde_json::from_str(&out_d).expect("doctor --json parses");
    assert_eq!(
        v["federation_remote_token_per_source"].as_u64(),
        Some(1),
        "the peer_db labelled remote resolves its OWN per-source token tier: {out_d}"
    );
    assert_eq!(
        v["federation_remote_token_shared"].as_u64(),
        Some(0),
        "the per-source token wins; no shared-tier remote here: {out_d}"
    );
}

/// Liveness regression (A2, no regression): a peer pulled from a foreign source
/// carrying a remote `host` is TTL-judged, NEVER pid-probed. We seed a foreign
/// store with a peer whose host is a different machine and a pid that is alive on
/// THIS box — if weave wrongly pid-probed it, it would call the peer online; the
/// correct behavior is to fall open to TTL (recent ⇒ online by recency, not pid).
/// The crux is that no `/proc/<pid>` probe runs for a foreign-host peer; this test
/// confirms the federated listing does not crash or mis-probe on a remote host.
#[test]
fn federation_remote_host_peer_is_ttl_judged_not_pid_probed() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_ok(&local, &["register", "--name", "localpeer"]);
    // A foreign peer registered by another "machine": force a DIFFERENT host so
    // the federated read sees a remote-host peer. Liveness must fall open to TTL
    // for a host != this_host (it cannot /proc-probe a remote PID).
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let (ok, out, err) = run_env(
        &local,
        &["peers", "--json"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    assert!(ok, "federated peers must succeed; stderr:\n{err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("peers --json parses");
    let arr = v.as_array().expect("peers --json is an array");
    let remote = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("remotepeer"))
        .expect("foreign peer surfaced via federation");
    assert_eq!(
        remote["host"].as_str(),
        Some("some-other-machine"),
        "foreign peer carries the remote host: {out}"
    );
    // The remote-host peer is TTL-judged (just registered ⇒ online by recency),
    // NOT pid-probed: a foreign-host peer must never trigger a /proc probe and the
    // listing must succeed regardless of whatever PID it carries.
    assert_eq!(
        remote["online"].as_bool(),
        Some(true),
        "a just-registered remote-host peer is TTL-online (recency, not pid): {out}"
    );
    assert!(
        arr.iter().any(|p| p["name"].as_str() == Some("localpeer")),
        "local peer present: {out}"
    );
}

/// Feature #6: `weave scan --json` surfaces a federated REMOTE-host peer with the
/// additive `remote:true` + `liveness:"alive_remote"` keys (TTL-judged, never
/// pid-probed), while leaving every pre-existing key intact (backward-compat).
/// Uses the proven forced-`HOSTNAME` + `WEAVE_PEER_DBS` foreign-store fixture so
/// it is hermetic: no wall-clock, no sleep, no backdate. A just-registered remote
/// row is recent ⇒ AliveRemote.
#[test]
fn scan_json_surfaces_remote_host_peer_alive_remote_additive_keys() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    // Register the local peer under the SAME forced host the scan will use, so it
    // is genuinely same-host (host is captured at registration time).
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    // Foreign peer "owned" by a different machine: force a different HOSTNAME so
    // the federated read sees a remote-host row.
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let (ok, out, err) = run_env(
        &local,
        &["scan", "--json"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    assert!(ok, "scan --json must succeed; stderr:\n{err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("scan --json parses");
    let arr = v.as_array().expect("scan --json is an array");

    let remote = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("remotepeer"))
        .unwrap_or_else(|| panic!("foreign remote peer surfaced via federation: {out}"));

    // The new additive keys.
    assert_eq!(
        remote["remote"].as_bool(),
        Some(true),
        "remotepeer host differs from this-machine => remote:true: {out}"
    );
    assert_eq!(
        remote["liveness"].as_str(),
        Some("alive_remote"),
        "a recent remote-host peer is alive_remote (TTL-judged, not pid-probed): {out}"
    );
    // Pre-existing keys UNCHANGED (backward-compat: same names + values).
    assert_eq!(remote["host"].as_str(), Some("some-other-machine"), "{out}");
    assert_eq!(
        remote["alive"].as_bool(),
        Some(true),
        "recent remote peer is alive (bool unchanged): {out}"
    );
    assert_eq!(
        remote["foreign"].as_bool(),
        Some(true),
        "federated row is foreign: {out}"
    );
    // Every documented key is present (no key dropped by the additive change).
    for k in [
        "name", "repo", "branch", "worktree", "mux", "pane", "host", "alive", "origin", "foreign",
        "liveness", "remote",
    ] {
        assert!(
            remote.get(k).is_some(),
            "scan --json row missing key {k}: {out}"
        );
    }
    // No secret/token bytes leak into scan output.
    assert!(
        !out.to_lowercase().contains("token") && !out.contains("libsql://"),
        "scan --json must be secret-free: {out}"
    );

    // The LOCAL peer is same-host => remote:false (deterministic). Its liveness is
    // alive_local OR stale depending on the registered pid nuance: `register`
    // stored that short-lived process's pid, which has exited by scan time, so on
    // Linux a same-host dead pid reads stale; either way it is NEVER alive_remote.
    let localp = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("localpeer"))
        .unwrap_or_else(|| panic!("local peer present: {out}"));
    assert_eq!(localp["remote"].as_bool(), Some(false), "{out}");
    let lv = localp["liveness"].as_str().unwrap_or("");
    assert!(
        lv == "alive_local" || lv == "stale",
        "same-host peer is alive_local or stale (pid nuance), never alive_remote: {out}"
    );
}

/// Feature #6: the HUMAN `weave scan` output shows the `<remote>` marker, the
/// `[alive (remote, ttl)]` reason, and a `summary:` line whose counts match the
/// rows (1 local-alive + 1 remote-alive). Same hermetic foreign fixture.
#[test]
fn scan_human_shows_remote_marker_reason_and_summary_counts() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let out = run_ok_env(
        &local,
        &["scan"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    // Remote marker + remote reason appear for the foreign-host row.
    assert!(
        out.contains("<remote>"),
        "human scan shows the remote marker: {out}"
    );
    assert!(
        out.contains("alive (remote, ttl)"),
        "human scan shows the remote TTL reason: {out}"
    );
    // The local row shows a local reason (pid-confirmed, this short-lived process
    // is the registered pid and may have exited — so accept either local reason
    // OR stale for the local row's PID nuance; the marker is what matters here).
    assert!(
        out.contains("localpeer"),
        "local peer present in human scan: {out}"
    );
    // Summary line with matching counts: exactly one remote-alive row.
    let summary = out
        .lines()
        .find(|l| l.starts_with("summary:"))
        .unwrap_or_else(|| panic!("summary line present: {out}"));
    assert!(
        summary.contains("1 remote-alive"),
        "summary counts the single remote-alive row: {summary}"
    );
    // Two rows total; the local row is either local-alive or stale (pid nuance),
    // but the remote count is deterministic.
    assert!(
        summary.contains("local-alive") && summary.contains("stale"),
        "summary lists all three buckets: {summary}"
    );
}

/// Backward-compat: a plain `weave scan --json` with NO federated/remote rows
/// behaves as before — the only peer is the local self row, `remote:false`,
/// `liveness:"alive_local"` (or stale via pid nuance), and the historical keys
/// are all present. No remote marker, single-host summary.
#[test]
fn scan_no_remote_rows_is_backward_compatible() {
    let db = TestDb::new();
    run_env(
        &db,
        &["register", "--name", "solo"],
        &[("HOSTNAME", "this-machine")],
    );
    let out = run_ok_env(&db, &["scan", "--json"], &[("HOSTNAME", "this-machine")]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("scan --json parses");
    let arr = v.as_array().expect("array");
    let solo = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("solo"))
        .unwrap_or_else(|| panic!("solo peer present: {out}"));
    assert_eq!(
        solo["remote"].as_bool(),
        Some(false),
        "no foreign host => remote:false: {out}"
    );
    assert_eq!(
        solo["foreign"].as_bool(),
        Some(false),
        "the local self row is not foreign: {out}"
    );
    // Human plain scan has no remote marker.
    let human = run_ok_env(&db, &["scan"], &[("HOSTNAME", "this-machine")]);
    assert!(
        !human.contains("<remote>"),
        "no remote marker without a remote row: {human}"
    );
    assert!(
        human.lines().any(|l| l.starts_with("summary:")),
        "summary line still printed for a single local row: {human}"
    );
}

/// Feature #6 MCP parity: the `weave_scan` tool mirrors the human surfacing —
/// a federated REMOTE-host peer shows the `<remote>` marker, the
/// `alive (remote, ttl)` reason, and the `summary:` line, all secret-free (no
/// token / no `libsql://` bytes). Hermetic forced-`HOSTNAME` + `WEAVE_PEER_DBS`.
#[test]
fn mcp_weave_scan_surfaces_remote_marker_reason_summary() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    let (err, text) = mcp.call_tool("weave_scan", serde_json::json!({}));
    assert!(!err, "weave_scan is not an error: {text}");
    assert!(
        text.contains("remotepeer"),
        "weave_scan lists the federated remote peer: {text}"
    );
    assert!(
        text.contains("<remote>"),
        "weave_scan mirrors the remote marker: {text}"
    );
    assert!(
        text.contains("alive (remote, ttl)"),
        "weave_scan mirrors the remote TTL reason: {text}"
    );
    assert!(
        text.contains("summary:") && text.contains("remote-alive"),
        "weave_scan mirrors the summary line: {text}"
    );
    // Secret-free: no token bytes, no remote URL scheme.
    assert!(
        !text.to_lowercase().contains("token") && !text.contains("libsql://"),
        "weave_scan output must be secret-free: {text}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Feature #8 — host-aware Liveness reason on peers / doctor / sessions --watch.
//
// Display-only, additive: the SAME #6 host-aware vocabulary (tokens
// alive_local|alive_remote|stale, the `<remote>` marker, the
// `alive (remote, ttl)` reason, the `N local-alive, M remote-alive, K stale`
// summary) is now surfaced on `peers`, `doctor`, and the `sessions --watch`
// dashboard. All hermetic & deterministic: reuse the proven forced-`HOSTNAME` +
// `WEAVE_PEER_DBS` foreign-store fixture (a just-registered remote-host row is
// recent ⇒ AliveRemote, never pid-probed cross-machine), and #5's bounded
// `--iterations 1` for the dashboard (a returning call proves termination).
//
// CONSISTENCY MANDATE: a peers/watch row's liveness/remote/reason must be
// BYTE-IDENTICAL to what `weave scan` emits for the SAME peer — asserted by
// comparing a scan row to a peers row directly.
// ---------------------------------------------------------------------------

/// `peers --json` carries the additive `liveness` token + `remote` bool per peer
/// (every pre-existing key intact), and a federated remote-host peer reads
/// `liveness:"alive_remote"` / `remote:true`. Hermetic foreign fixture.
#[test]
fn peers_json_surfaces_remote_host_peer_alive_remote_additive_keys() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let out = run_ok_env(
        &local,
        &["peers", "--json"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("peers --json parses");
    let arr = v.as_array().expect("peers --json is an array");

    let remote = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("remotepeer"))
        .unwrap_or_else(|| panic!("foreign remote peer surfaced via federation: {out}"));
    assert_eq!(
        remote["remote"].as_bool(),
        Some(true),
        "remotepeer host differs => remote:true: {out}"
    );
    assert_eq!(
        remote["liveness"].as_str(),
        Some("alive_remote"),
        "a recent remote-host peer is alive_remote (TTL-judged): {out}"
    );
    // Pre-existing keys still present (additive change does not drop any key).
    for k in [
        "name",
        "mux",
        "target",
        "socket",
        "cwd",
        "host",
        "injectable",
        "origin",
        "foreign",
        "liveness",
        "remote",
    ] {
        assert!(
            remote.get(k).is_some(),
            "peers --json row missing key {k}: {out}"
        );
    }
    // The local self row is same-host => remote:false, never alive_remote.
    let localp = arr
        .iter()
        .find(|p| p["name"].as_str() == Some("localpeer"))
        .unwrap_or_else(|| panic!("local peer present: {out}"));
    assert_eq!(localp["remote"].as_bool(), Some(false), "{out}");
    let lv = localp["liveness"].as_str().unwrap_or("");
    assert!(
        lv == "alive_local" || lv == "stale",
        "same-host peer is alive_local or stale (pid nuance), never alive_remote: {out}"
    );
    // Secret-free.
    assert!(
        !out.to_lowercase().contains("token") && !out.contains("libsql://"),
        "peers --json must be secret-free: {out}"
    );
}

/// CONSISTENCY: a `peers --json` row's `liveness`/`remote` are byte-identical to
/// the `scan --json` row's for the SAME peer (the #6 vocabulary is reused, not
/// forked). Same hermetic foreign fixture and forced host for both invocations.
#[test]
fn peers_json_liveness_keys_match_scan_for_same_peer() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );
    let env = [
        ("WEAVE_PEER_DBS", foreign.path_str()),
        ("HOSTNAME", "this-machine".to_string()),
    ];
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let scan_out = run_ok_env(&local, &["scan", "--json"], &env_ref);
    let peers_out = run_ok_env(&local, &["peers", "--json"], &env_ref);
    let scan: serde_json::Value = serde_json::from_str(&scan_out).expect("scan --json");
    let peers: serde_json::Value = serde_json::from_str(&peers_out).expect("peers --json");

    let find = |v: &serde_json::Value, name: &str| -> serde_json::Value {
        v.as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("{name} present"))
            .clone()
    };
    let s_remote = find(&scan, "remotepeer");
    let p_remote = find(&peers, "remotepeer");
    assert_eq!(
        s_remote["liveness"], p_remote["liveness"],
        "scan vs peers liveness token must be identical for the same peer:\nscan={scan_out}\npeers={peers_out}"
    );
    assert_eq!(
        s_remote["remote"], p_remote["remote"],
        "scan vs peers `remote` flag must be identical for the same peer"
    );
    assert_eq!(p_remote["liveness"].as_str(), Some("alive_remote"));
    assert_eq!(p_remote["remote"].as_bool(), Some(true));
}

/// Human `weave peers` shows the `<remote>` marker + `[alive (remote, ttl)]`
/// reason for the federated remote-host row — the same idiom `scan` prints.
#[test]
fn peers_human_shows_remote_marker_and_reason() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );
    let out = run_ok_env(
        &local,
        &["peers"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );
    assert!(
        out.contains("<remote>"),
        "human peers shows remote marker: {out}"
    );
    assert!(
        out.contains("alive (remote, ttl)"),
        "human peers shows the remote TTL reason: {out}"
    );
}

/// `doctor --json` carries the additive `peers_alive_local` /
/// `peers_alive_remote` / `peers_stale` counts; with one federated remote-host
/// row the remote count is exactly 1, and the human `doctor` shows the
/// `N local-alive, M remote-alive, K stale` line. Pre-existing keys intact.
#[test]
fn doctor_json_and_human_surface_liveness_counts() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );
    let foreign_path = foreign.path_str();
    let env = [
        ("WEAVE_PEER_DBS", foreign_path.as_str()),
        ("HOSTNAME", "this-machine"),
    ];

    let out = run_ok_env(&local, &["doctor", "--json"], &env);
    let v: serde_json::Value = serde_json::from_str(&out).expect("doctor --json parses");
    // Additive count keys present, siblings of the pre-existing peer counts.
    for k in [
        "peers",
        "peers_online",
        "peers_alive_local",
        "peers_alive_remote",
        "peers_stale",
    ] {
        assert!(v.get(k).is_some(), "doctor --json missing key {k}: {out}");
    }
    // Exactly one federated remote-host row ⇒ remote-alive count is 1.
    assert_eq!(
        v["peers_alive_remote"].as_u64(),
        Some(1),
        "one recent remote-host peer => peers_alive_remote == 1: {out}"
    );
    // The three liveness buckets partition the peer set.
    let total = v["peers"].as_u64().unwrap_or(0);
    let sum = v["peers_alive_local"].as_u64().unwrap_or(0)
        + v["peers_alive_remote"].as_u64().unwrap_or(0)
        + v["peers_stale"].as_u64().unwrap_or(0);
    assert_eq!(
        total, sum,
        "liveness buckets must sum to peers total: {out}"
    );

    // Human: the three-count summary line, exact #6 phrasing.
    let human = run_ok_env(&local, &["doctor"], &env);
    assert!(
        human.contains("1 remote-alive"),
        "human doctor shows the remote-alive count: {human}"
    );
    assert!(
        human.contains("local-alive") && human.contains("stale"),
        "human doctor lists all three liveness buckets: {human}"
    );
}

/// `sessions --watch --iterations 1` (#5 bounded path) over a mix of a local row
/// and a federated remote-host row shows the per-row reason marker, the
/// `<remote>` marker for the remote row, and the three-count header — and STILL
/// EXITS (a returning call proves the loop terminated; no hang).
#[test]
fn sessions_watch_shows_liveness_reasons_and_remote_marker() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &local,
        &["register", "--name", "localpeer"],
        &[("HOSTNAME", "this-machine")],
    );
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );

    let (ok, out, err) = run_env(
        &local,
        &["sessions", "--watch", "--iterations", "1"],
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
            ("WEAVE_NO_CLEAR", "1"),
            ("NO_COLOR", "1"),
        ],
    );
    assert!(
        ok,
        "watch --iterations 1 must exit 0 (no hang); stderr: {err}"
    );
    assert_eq!(frame_count(&out), 1, "exactly one frame:\n{out}");
    // Remote row carries the marker + the remote TTL reason.
    assert!(out.contains("remotepeer"), "remote peer present: {out}");
    assert!(
        out.contains("<remote>"),
        "watch shows the remote marker: {out}"
    );
    assert!(
        out.contains("[alive (remote, ttl)]"),
        "watch shows the remote TTL reason for the remote row: {out}"
    );
    // Header three-count breakdown (the #6 phrasing), with the single remote row.
    assert!(
        out.contains("1 remote-alive"),
        "watch header shows the remote-alive count: {out}"
    );
    assert!(
        out.contains("local-alive") && out.contains("stale,"),
        "watch header lists all three liveness buckets: {out}"
    );
    assert!(
        !out.as_bytes().contains(&0x1b),
        "plain watch frame must carry no ANSI escape: {out:?}"
    );
}

/// CONSISTENCY: the `sessions --watch --json` snapshot's `liveness`/`remote` keys
/// are byte-identical to the `scan --json` row's for the SAME remote peer (the
/// watch JSON reuses the #6 vocabulary, not a forked one). Hermetic fixture.
#[test]
fn sessions_watch_json_liveness_matches_scan_for_same_peer() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );
    let foreign_path = foreign.path_str();
    let env = [
        ("WEAVE_PEER_DBS", foreign_path.as_str()),
        ("HOSTNAME", "this-machine"),
        ("WEAVE_NO_CLEAR", "1"),
        ("NO_COLOR", "1"),
    ];
    let scan_out = run_ok_env(&local, &["scan", "--json"], &env);
    let watch_out = run_ok_env(&local, &["sessions", "--watch", "--json"], &env);
    let scan: serde_json::Value = serde_json::from_str(&scan_out).expect("scan --json");
    let watch: serde_json::Value = serde_json::from_str(&watch_out).expect("watch --json");

    let s = scan
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("remotepeer"))
        .unwrap_or_else(|| panic!("scan has remotepeer: {scan_out}"))
        .clone();
    let w = watch
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some("remotepeer"))
        .unwrap_or_else(|| panic!("watch json has remotepeer: {watch_out}"))
        .clone();
    assert_eq!(
        s["liveness"], w["liveness"],
        "scan vs watch liveness token identical for the same peer:\nscan={scan_out}\nwatch={watch_out}"
    );
    assert_eq!(
        s["remote"], w["remote"],
        "scan vs watch `remote` flag identical for the same peer"
    );
    assert_eq!(w["liveness"].as_str(), Some("alive_remote"));
}

/// MCP parity (#8): `weave_peers` shows the `<remote>` marker + reason and
/// `weave_doctor` shows the three-count summary — mirroring the CLI, secret-free,
/// returned as a String (stdout discipline). Hermetic foreign fixture.
#[test]
fn mcp_weave_peers_and_doctor_surface_liveness() {
    let local = TestDb::new();
    let foreign = TestDb::new();
    run_env(
        &foreign,
        &["register", "--name", "remotepeer"],
        &[("HOSTNAME", "some-other-machine")],
    );
    let mut mcp = McpServer::spawn_env(
        &local,
        &[
            ("WEAVE_PEER_DBS", &foreign.path_str()),
            ("HOSTNAME", "this-machine"),
        ],
    );

    let (err, text) = mcp.call_tool("weave_peers", serde_json::json!({}));
    assert!(!err, "weave_peers not an error: {text}");
    assert!(
        text.contains("remotepeer"),
        "weave_peers lists remote peer: {text}"
    );
    assert!(
        text.contains("<remote>"),
        "weave_peers mirrors the remote marker: {text}"
    );
    assert!(
        text.contains("alive (remote, ttl)"),
        "weave_peers mirrors the remote TTL reason: {text}"
    );
    assert!(
        !text.to_lowercase().contains("token") && !text.contains("libsql://"),
        "weave_peers secret-free: {text}"
    );

    let (err, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!err, "weave_doctor not an error: {dtext}");
    assert!(
        dtext.contains("1 remote-alive"),
        "weave_doctor shows the remote-alive count: {dtext}"
    );
    assert!(
        dtext.contains("local-alive") && dtext.contains("stale"),
        "weave_doctor lists all three liveness buckets: {dtext}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Tier-2 v2 — LIVE remote (Turso) pull. ENV-GATED + `#[ignore]`: CI never sets
// the env and never passes `--ignored`, so the default suite stays HERMETIC (no
// network). Run manually against a real Turso DB only (see docs/TESTING.md):
//
//   WEAVE_TEST_TURSO_URL=libsql://<db>.turso.io \
//   WEAVE_TEST_TURSO_TOKEN=<read-only-token> \
//     cargo test --no-default-features --features libsql -- --ignored remote_live
//
// The hermetic write-guard unit test
// (`store_libsql::tests::read_only_handle_traps_every_write_and_leaves_file_unchanged`)
// is the unattended proof of OWNER-ONLY-WRITES; this is the optional real-Turso
// smoke that delivery + idempotency hold cross-machine.
// ---------------------------------------------------------------------------

/// LIVE: pull from a real remote Turso outbox into the local inbox, then assert a
/// re-pull is idempotent. Inert unless both env vars are set (and built with
/// `--features libsql`); `#[ignore]` so it never runs in the default `cargo test`.
#[cfg(feature = "libsql")]
#[test]
#[ignore = "live remote Turso test; set WEAVE_TEST_TURSO_URL/_TOKEN and run with --ignored"]
fn remote_live_pull_delivers_and_is_idempotent() {
    let url = match std::env::var("WEAVE_TEST_TURSO_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!("WEAVE_TEST_TURSO_URL unset; skipping live remote test");
            return;
        }
    };
    let token = std::env::var("WEAVE_TEST_TURSO_TOKEN").unwrap_or_default();

    // Local receiver pulls FROM the remote outbox. The remote must already carry an
    // intent addressed to `bob` (seed it out-of-band per the manual procedure).
    let b = TestDb::new();
    run_ok(&b, &["register", "--name", "bob"]);

    let env = [
        ("WEAVE_PULL_FROM", url.as_str()),
        ("WEAVE_PULL_TOKEN", token.as_str()),
        ("WEAVE_PULL_TIMEOUT_MS", "5000"),
    ];
    // First pull commits whatever the remote outbox holds for bob.
    let _ = run_ok_env(&b, &["pull", "--me", "bob"], &env);
    let inbox1 = run_ok_env(&b, &["inbox", "--me", "bob", "--all", "--json"], &env);

    // A second pull must be idempotent: the cursor is local, so nothing re-delivers.
    let _ = run_ok_env(&b, &["pull", "--me", "bob"], &env);
    let inbox2 = run_ok_env(&b, &["inbox", "--me", "bob", "--all", "--json"], &env);

    let n1 = serde_json::from_str::<serde_json::Value>(&inbox1)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0);
    let n2 = serde_json::from_str::<serde_json::Value>(&inbox2)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0);
    assert_eq!(n1, n2, "a second remote pull must not double-deliver");
    // The token must never appear in any output.
    assert!(!inbox1.contains(token.as_str()) || token.is_empty());
}

// ---------------------------------------------------------------------------
// N. Session scan / identify / tag (repo · branch · worktree_id)
//
// All hermetic: a temp cwd carrying a crafted `.git` FILE (the linked-worktree
// shape) makes the captured worktree_id deterministic with NO real repo and NO
// `git` binary. branch/repo need real `git` and are intentionally left empty in
// these fixtures (covered by the pure-parse units + the gated real-git asserts).
// ---------------------------------------------------------------------------

/// Make a temp dir whose `.git` is a FILE pointing at a linked worktree named
/// `wt`, so cwd-derived tagging yields `worktree_id == wt` without a git binary.
/// The dir is unique per call and cleaned up by the caller's `TempCwd` guard.
fn linked_worktree_cwd(wt: &str) -> TempCwd {
    let dir = std::env::temp_dir().join(format!(
        "weave-scan-cwd-{}-{}-{}",
        std::process::id(),
        wt,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp cwd");
    std::fs::write(
        dir.join(".git"),
        format!("gitdir: /fixture/main/.git/worktrees/{wt}/.git\n"),
    )
    .expect("write crafted .git file");
    TempCwd { path: dir }
}

/// A temp cwd that removes itself on drop.
struct TempCwd {
    path: std::path::PathBuf,
}
impl Drop for TempCwd {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `weave register` (from a crafted linked-worktree cwd) then `weave scan` shows
/// the row, and `weave scan --json` carries the full additive shape.
#[test]
fn scan_lists_registered_peer_with_tags_and_json_shape() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("scan-wt");

    // Register a peer from the crafted cwd so its worktree_id tag is captured.
    let (ok, _o, err) = run_in_cwd(&db, &["register", "--name", "alpha"], &cwd.path);
    assert!(ok, "register failed: {err}");

    // Human `weave scan` shows the row with its worktree tag.
    let human = run_ok(&db, &["scan"]);
    assert!(
        human.contains("alpha"),
        "scan human lists the peer: {human}"
    );
    assert!(
        human.contains("worktree=scan-wt"),
        "scan human shows the captured worktree id: {human}"
    );

    // `weave scan --json` carries the documented additive shape.
    let out = run_ok(&db, &["scan", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("scan --json parses");
    let arr = v.as_array().expect("scan --json is an array");
    let row = arr
        .iter()
        .find(|p| p["name"] == "alpha")
        .expect("scanned peer present");
    for key in [
        "name", "repo", "branch", "worktree", "mux", "pane", "host", "alive", "origin", "foreign",
    ] {
        assert!(row.get(key).is_some(), "scan row has key {key:?}: {row}");
    }
    assert_eq!(row["worktree"], "scan-wt", "worktree tag roundtrips: {row}");
    assert_eq!(row["foreign"], false, "self row is local/not foreign");
}

/// `--repo` / `--branch` filters narrow the scan set by exact tag match. We drive
/// the tags deterministically by registering peers that carry explicit tags via a
/// crafted cwd for worktree_id; repo/branch filters are exercised against the
/// empty-tag default (a non-matching filter yields no rows; an empty match keeps
/// rows out — proving the filter is applied, never ignored).
#[test]
fn scan_repo_and_branch_filters_narrow_the_set() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("filter-wt");
    run_in_cwd(&db, &["register", "--name", "beta"], &cwd.path);

    // No filter: the peer is present.
    let all = run_ok(&db, &["scan", "--json"]);
    let all_v: serde_json::Value = serde_json::from_str(&all).unwrap();
    assert_eq!(all_v.as_array().unwrap().len(), 1, "one peer unfiltered");

    // A repo filter that cannot match the (empty) repo tag drops the row.
    let none = run_ok(&db, &["scan", "--repo", "no-such-repo", "--json"]);
    let none_v: serde_json::Value = serde_json::from_str(&none).unwrap();
    assert_eq!(
        none_v.as_array().unwrap().len(),
        0,
        "a non-matching --repo filter narrows the set to empty: {none}"
    );

    // Likewise a non-matching branch filter.
    let none_b = run_ok(&db, &["scan", "--branch", "no-such-branch", "--json"]);
    let none_b_v: serde_json::Value = serde_json::from_str(&none_b).unwrap();
    assert_eq!(
        none_b_v.as_array().unwrap().len(),
        0,
        "a non-matching --branch filter narrows the set to empty: {none_b}"
    );
}

/// Tags surface on `peers --json`, on the new `sessions` display-join, and on
/// `doctor` — the three additional tag-display surfaces.
#[test]
fn tags_visible_in_peers_sessions_and_doctor() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("surf-wt");
    run_in_cwd(&db, &["register", "--name", "gamma"], &cwd.path);
    // A message so `gamma` shows up as a session (sessions are message-derived).
    run_ok(
        &db,
        &[
            "send",
            "--from",
            "gamma",
            "--to",
            "gamma",
            "--body",
            "self ping",
        ],
    );

    // peers --json carries the tag fields.
    let peers = run_ok(&db, &["peers", "--json"]);
    let pv: serde_json::Value = serde_json::from_str(&peers).expect("peers --json parses");
    let prow = pv
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "gamma")
        .expect("peer present");
    assert_eq!(prow["worktree"], "surf-wt", "peers --json has worktree tag");
    assert!(prow.get("repo").is_some() && prow.get("branch").is_some());

    // sessions --json carries the display-joined tags (leader refinement #1).
    let sessions = run_ok(&db, &["sessions", "--json"]);
    let sv: serde_json::Value = serde_json::from_str(&sessions).expect("sessions --json parses");
    let srow = sv
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "gamma")
        .expect("session present");
    assert_eq!(
        srow["worktree"], "surf-wt",
        "sessions --json display-joins the local peer's worktree tag: {srow}"
    );
    assert!(
        srow.get("repo").is_some() && srow.get("branch").is_some(),
        "sessions --json carries repo/branch keys: {srow}"
    );

    // sessions (human) shows the tag suffix from the join.
    let sessions_human = run_ok(&db, &["sessions"]);
    assert!(
        sessions_human.contains("surf-wt"),
        "sessions human display-joins the tag: {sessions_human}"
    );

    // doctor surfaces a tagged-peers count.
    let doctor = run_ok(&db, &["doctor", "--json"]);
    let dv: serde_json::Value = serde_json::from_str(&doctor).expect("doctor --json parses");
    assert!(
        dv.get("peers_tagged").is_some(),
        "doctor --json reports a peers_tagged count: {dv}"
    );
}

/// A session with NO registered local peer shows empty/`-` tags in the join
/// (graceful degradation — the refinement must not error or fabricate tags).
#[test]
fn sessions_without_peer_show_empty_tags() {
    let db = TestDb::new();
    // A message creates a session 'lonely' with no peer row registered.
    run_ok(
        &db,
        &["send", "--from", "lonely", "--to", "lonely", "--body", "hi"],
    );
    let sessions = run_ok(&db, &["sessions", "--json"]);
    let sv: serde_json::Value = serde_json::from_str(&sessions).expect("sessions --json parses");
    let srow = sv
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "lonely")
        .expect("session present");
    assert_eq!(srow["repo"], "", "no-peer session has empty repo tag");
    assert_eq!(srow["branch"], "", "no-peer session has empty branch tag");
    assert_eq!(
        srow["worktree"], "",
        "no-peer session has empty worktree tag"
    );
}

/// MCP: `weave_scan` is advertised in `tools/list`, runs without error and shows
/// the joined tags, and tolerates an oversized/control-bearing `repo` filter arg
/// gracefully (no panic, no crash) — and `tool_sessions` shows the joined tags.
#[test]
fn mcp_weave_scan_listed_runs_and_filters_safely() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("mcp-wt");
    // Register a peer named after the server's cwd basename so the MCP server's
    // own identity resolves to it and tool_sessions/scan can join tags.
    let me = cwd.path.file_name().unwrap().to_string_lossy().into_owned();
    run_in_cwd(&db, &["register", "--name", &me], &cwd.path);
    // A message so `me` is a known session for tool_sessions.
    run_ok(&db, &["send", "--from", &me, "--to", &me, "--body", "ping"]);

    let mut srv = McpServer::spawn_full(&db, &["mcp"], &[], Some(&cwd.path));
    let _ = srv.request("initialize", serde_json::json!({}));

    // weave_scan appears in tools/list.
    let tools = srv.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = tools["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "weave_scan"),
        "weave_scan advertised in tools/list: {names:?}"
    );

    // call_tool weave_scan {} -> not an error, shows the captured worktree tag.
    let (is_err, text) = srv.call_tool("weave_scan", serde_json::json!({}));
    assert!(!is_err, "weave_scan returned isError: {text}");
    assert!(
        text.contains("mcp-wt"),
        "weave_scan shows the joined worktree tag: {text}"
    );

    // An oversized + control-bearing repo filter arg is handled gracefully:
    // never a panic, never a server crash. (It may be rejected as a bad filter or
    // simply match nothing — either is fine; what matters is no crash/hang.)
    let hostile = format!("{}\n\t;`$(rm -rf /)", "A".repeat(5000));
    let (_is_err2, _t2) = srv.call_tool("weave_scan", serde_json::json!({ "repo": hostile }));
    // The server must still be alive and responsive afterward.
    let (is_err3, _t3) = srv.call_tool("weave_scan", serde_json::json!({}));
    assert!(
        !is_err3,
        "server still serves weave_scan after a hostile filter arg"
    );

    // tool_sessions shows the display-joined tag too.
    let (sess_err, sess_text) = srv.call_tool("weave_sessions", serde_json::json!({}));
    assert!(!sess_err, "weave_sessions errored: {sess_text}");
    assert!(
        sess_text.contains("mcp-wt"),
        "tool_sessions display-joins the worktree tag: {sess_text}"
    );

    srv.shutdown();
}

/// Real-`git` smoke (GATED): when a `git` binary is present, a genuine `git init`
/// checkout yields a non-empty repo + branch tag through `weave register`/`scan`.
/// Skipped entirely when no `git` is available so zero-git CI still passes.
#[test]
fn scan_real_git_repo_tags_when_git_present() {
    // Mirror the binary's trusted-path resolution: only run if `git` is on PATH.
    if which_git().is_none() {
        eprintln!("skipping: no `git` binary available");
        return;
    }
    let git = which_git().unwrap();
    let db = TestDb::new();
    let dir = std::env::temp_dir().join(format!(
        "weave-scan-realgit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let guard = TempCwd { path: dir.clone() };
    // Initialize a real repo on a known branch with no network, no remote.
    let run_git = |args: &[&str]| {
        Command::new(&git)
            .args(args)
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(run_git(&["init", "-q", "-b", "trunk"]) || run_git(&["init", "-q"]));

    run_in_cwd(&db, &["register", "--name", "realrepo"], &guard.path);
    let out = run_ok(&db, &["scan", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "realrepo")
        .expect("real repo peer present");
    // The main worktree's id is the "(main)" sentinel; repo basename is captured.
    assert_eq!(row["worktree"], "(main)", "main worktree sentinel: {row}");
    assert!(
        row["repo"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "real git yields a non-empty repo tag: {row}"
    );
}

/// Resolve a `git` binary the SAME way the binary's `inject::resolve_trusted` does
/// — by absolute path inside the trusted system/user-tool dirs (NOT ambient PATH),
/// so this gate matches whether `weave register` will actually capture branch/repo.
fn which_git() -> Option<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> =
        ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let h = std::path::PathBuf::from(home);
        dirs.push(h.join(".cargo/bin"));
        dirs.push(h.join(".local/bin"));
        dirs.push(h.join(".nix-profile/bin"));
    }
    dirs.into_iter()
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// O. Presence dashboard: `weave sessions --watch` (Feature #5)
//
// All hermetic & NON-HANGING: every watch run is bounded by `--iterations N`
// (NEVER a wall-clock sleep assertion). `run`/`run_ok` only return once the
// child has EXITED, so a returning call is itself proof the loop terminated —
// a true hang would wedge the test, not silently pass. `WEAVE_NO_CLEAR=1` keeps
// frames escape-byte-free regardless of the runner's TTY state. Peers carry
// deterministic worktree tags via a crafted `.git` FILE (no real git / no
// network), grouping under `[- / -]`; a separate gated test covers real
// repo/branch grouping when a `git` binary is present.
// ---------------------------------------------------------------------------

/// Extra env that forces the watch dashboard into its plain (escape-free) path
/// no matter what terminal the test runner has, so captured stdout is stable.
const NO_CLEAR: [(&str, &str); 2] = [("WEAVE_NO_CLEAR", "1"), ("NO_COLOR", "1")];

/// Count rendered frames by the per-frame header line the renderer always emits.
fn frame_count(stdout: &str) -> usize {
    stdout.matches("weave sessions [").count()
}

/// `weave sessions --watch --iterations 1` renders EXACTLY ONE frame, EXITS 0
/// (no hang — the harness returning proves termination, bounded by iteration
/// count not a sleep), and the frame carries the grouped section + header
/// summary + both peers' tags. A tiny `--interval` is irrelevant because the
/// single-iteration path never sleeps (sleep is BETWEEN frames only).
#[test]
fn sessions_watch_iterations_one_emits_one_frame_and_exits() {
    let db = TestDb::new();
    let cwd_a = linked_worktree_cwd("watch-wt-a");
    let cwd_b = linked_worktree_cwd("watch-wt-b");
    run_in_cwd(&db, &["register", "--name", "wone"], &cwd_a.path);
    run_in_cwd(&db, &["register", "--name", "wtwo"], &cwd_b.path);

    let (ok, out, err) = run_env(
        &db,
        &[
            "sessions",
            "--watch",
            "--iterations",
            "1",
            "--interval",
            "1",
        ],
        &NO_CLEAR,
    );
    assert!(
        ok,
        "watch --iterations 1 must exit 0 (no hang); stderr: {err}"
    );
    // Exactly one frame.
    assert_eq!(frame_count(&out), 1, "expected exactly one frame:\n{out}");
    // Header summary present (presence-focused counts).
    assert!(
        out.contains("session(s),") && out.contains("alive,") && out.contains("repo(s),"),
        "frame missing header summary:\n{out}"
    );
    // Both peers appear, with their deterministic worktree tags, under the
    // empty-repo/branch group section.
    assert!(out.contains("wone"), "frame missing peer wone:\n{out}");
    assert!(out.contains("wtwo"), "frame missing peer wtwo:\n{out}");
    assert!(
        out.contains("worktree=watch-wt-a") && out.contains("worktree=watch-wt-b"),
        "frame missing worktree tags:\n{out}"
    );
    assert!(
        out.contains("[- / -]"),
        "frame missing the grouped section header:\n{out}"
    );
    // Plain path: no escape bytes leak into captured stdout.
    assert!(
        !out.as_bytes().contains(&0x1b),
        "plain watch frame must carry no ANSI escape:\n{out:?}"
    );
}

/// `--iterations 2` emits TWO frames and still exits. This is the multi-tick
/// non-hang proof: the loop sleeps `--interval` BETWEEN the two frames then
/// returns (never an infinite loop, never a post-last sleep).
#[test]
fn sessions_watch_iterations_two_emits_two_frames_and_exits() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("watch-2x");
    run_in_cwd(&db, &["register", "--name", "twice"], &cwd.path);

    let (ok, out, err) = run_env(
        &db,
        &[
            "sessions",
            "--watch",
            "--iterations",
            "2",
            "--interval",
            "1",
        ],
        &NO_CLEAR,
    );
    assert!(
        ok,
        "watch --iterations 2 must exit 0 (no hang); stderr: {err}"
    );
    assert_eq!(frame_count(&out), 2, "expected exactly two frames:\n{out}");
}

/// `weave sessions --watch --json` emits a SINGLE JSON snapshot and exits — no
/// loop, no clear prefix, no escape bytes — and the array carries the presence
/// shape (name/repo/branch/worktree/alive/via), never a token or URL.
#[test]
fn sessions_watch_json_single_snapshot_and_exits() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("watch-json");
    run_in_cwd(&db, &["register", "--name", "jsonpeer"], &cwd.path);

    let out = run_ok_env(&db, &["sessions", "--watch", "--json"], &NO_CLEAR);
    // No clear-screen / ANSI escape in JSON mode, ever.
    assert!(
        !out.as_bytes().contains(&0x1b),
        "watch --json must carry no ANSI escape:\n{out:?}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&out).expect("watch --json emits a single JSON snapshot");
    let arr = v.as_array().expect("watch --json is an array");
    let row = arr
        .iter()
        .find(|r| r["name"] == "jsonpeer")
        .expect("registered peer present in watch --json");
    for key in [
        "name", "repo", "branch", "worktree", "mux", "host", "alive", "via",
    ] {
        assert!(
            row.get(key).is_some(),
            "watch json row has key {key:?}: {row}"
        );
    }
    assert_eq!(
        row["worktree"], "watch-json",
        "worktree tag roundtrips: {row}"
    );
    // Secret-free: no token/URL shape leaks via `via` for a purely-local peer.
    assert_eq!(row["via"], "", "local peer has an empty `via` label: {row}");
}

/// A `--repo` filter that matches NOTHING narrows the watch frame to the stable
/// empty body (`no sessions`) and still exits — proving the #1 exact-match
/// filter composes with `--watch` and is never ignored.
#[test]
fn sessions_watch_nonmatching_repo_filter_narrows_to_empty() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("watch-filter");
    run_in_cwd(&db, &["register", "--name", "filt"], &cwd.path);

    // Unfiltered: the peer is present.
    let all = run_env(
        &db,
        &["sessions", "--watch", "--iterations", "1"],
        &NO_CLEAR,
    );
    assert!(
        all.1.contains("filt"),
        "unfiltered watch lists the peer:\n{}",
        all.1
    );

    // A repo filter that cannot match the (empty) repo tag yields the empty body.
    let (ok, out, err) = run_env(
        &db,
        &[
            "sessions",
            "--watch",
            "--iterations",
            "1",
            "--repo",
            "no-such-repo",
        ],
        &NO_CLEAR,
    );
    assert!(ok, "filtered watch must exit 0; stderr: {err}");
    assert!(
        out.contains("no sessions") && !out.contains("filt"),
        "a non-matching --repo filter must narrow the watch frame to empty:\n{out}"
    );
    assert!(
        out.contains("repo=no-such-repo"),
        "filter echoed in header:\n{out}"
    );
}

/// REGRESSION: plain `weave sessions` and `weave sessions --json` (NO `--watch`)
/// behave EXACTLY as before — the #1 display-join tag shape is intact and the
/// non-watch path renders the legacy unread-oriented session listing, never the
/// dashboard. (Guards against the new arm hijacking the default path.)
#[test]
fn sessions_without_watch_unchanged_regression() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("nowatch-wt");
    run_in_cwd(&db, &["register", "--name", "legacy"], &cwd.path);
    // A message so `legacy` is a session (sessions are message-derived).
    run_ok(
        &db,
        &[
            "send", "--from", "legacy", "--to", "legacy", "--body", "ping",
        ],
    );

    // Human: the legacy "N unread (last …)" line, NOT a dashboard header.
    let human = run_ok(&db, &["sessions"]);
    assert!(
        human.contains("legacy") && human.contains("unread"),
        "legacy sessions human line intact: {human}"
    );
    assert!(
        !human.contains("weave sessions ["),
        "non-watch sessions must NOT render a dashboard frame: {human}"
    );

    // JSON: the documented legacy session-array shape (unread/last_activity +
    // the #1 display-joined repo/branch/worktree), unchanged.
    let out = run_ok(&db, &["sessions", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("sessions --json parses");
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "legacy")
        .expect("session present");
    for key in [
        "name",
        "unread",
        "last_activity",
        "repo",
        "branch",
        "worktree",
        "origin",
        "foreign",
    ] {
        assert!(
            row.get(key).is_some(),
            "legacy sessions json key {key:?}: {row}"
        );
    }
    assert_eq!(
        row["worktree"], "nowatch-wt",
        "display-joined tag intact: {row}"
    );
}

/// READ-ONLY proof (owner-only-writes): the watch loop writes NOTHING per tick.
/// We snapshot the WEAVE_DB bytes (+ `-wal`) before and after a 3-iteration run
/// driven with NO explicit identity (so even the optional pre-loop self-refresh
/// is skipped) and assert the store is byte-identical across every tick.
#[test]
fn sessions_watch_is_read_only_store_unchanged_across_ticks() {
    let db = TestDb::new();
    let cwd = linked_worktree_cwd("readonly-wt");
    run_in_cwd(&db, &["register", "--name", "observer"], &cwd.path);

    // Snapshot every sqlite file for this DB before the watch run.
    let snapshot = || -> Vec<(String, Vec<u8>)> {
        let base = db.path.to_string_lossy().into_owned();
        ["", "-wal", "-shm", "-journal"]
            .iter()
            .filter_map(|suf| {
                let p = format!("{base}{suf}");
                std::fs::read(&p).ok().map(|bytes| (p, bytes))
            })
            .collect()
    };
    let before = snapshot();
    assert!(!before.is_empty(), "store file must exist after register");

    // 3 ticks, no explicit --me/--session (scrub_env clears WEAVE_SESSION) so the
    // owner self-refresh is skipped: every tick must be a pure read.
    let (ok, _out, err) = run_env(
        &db,
        &[
            "sessions",
            "--watch",
            "--iterations",
            "3",
            "--interval",
            "1",
        ],
        &NO_CLEAR,
    );
    assert!(ok, "read-only watch must exit 0; stderr: {err}");

    let after = snapshot();
    assert_eq!(
        before, after,
        "watch loop must not write to the store on any tick (owner-only-writes)"
    );
}

/// GATED real-git variant: when a `git` binary is present, two peers in DISTINCT
/// real repos on distinct branches render as DISTINCT `[repo / branch]` sections
/// in the watch frame — the genuine repo→branch grouping proof. Skipped with no
/// `git` so zero-git CI still passes (the hermetic tests above cover the rest).
#[test]
fn sessions_watch_groups_distinct_repos_with_real_git() {
    let git = match which_git() {
        Some(g) => g,
        None => {
            eprintln!("skipping: no `git` binary available");
            return;
        }
    };
    let db = TestDb::new();
    let mk_repo = |slug: &str, branch: &str| -> TempCwd {
        let dir = std::env::temp_dir().join(format!(
            "weave-watch-git-{}-{}-{}",
            std::process::id(),
            slug,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run_git = |args: &[&str]| {
            Command::new(&git)
                .args(args)
                .current_dir(&dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        assert!(run_git(&["init", "-q", "-b", branch]) || run_git(&["init", "-q"]));
        TempCwd { path: dir }
    };
    let repo_a = mk_repo("alpharepo", "trunk");
    let repo_b = mk_repo("betarepo", "trunk");
    let name_a = repo_a
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let name_b = repo_b
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    run_in_cwd(&db, &["register", "--name", &name_a], &repo_a.path);
    run_in_cwd(&db, &["register", "--name", &name_b], &repo_b.path);

    let (ok, out, err) = run_env(
        &db,
        &["sessions", "--watch", "--iterations", "1"],
        &NO_CLEAR,
    );
    assert!(ok, "real-git watch must exit 0; stderr: {err}");
    // Two distinct repo basenames ⇒ at least two distinct repo group sections,
    // and the header reports ≥2 repos.
    assert!(
        out.contains(&name_a) && out.contains(&name_b),
        "both real-repo peers present in the frame:\n{out}"
    );
    // Each peer's repo basename names its own group section header `[<repo> / …]`.
    assert!(
        out.contains(&format!("[{name_a} / ")) && out.contains(&format!("[{name_b} / ")),
        "distinct [repo / branch] sections expected:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Feature #7 — MULTI-KEY REGISTRY rotation overlap, end-to-end through the
// COMPILED binary. The HEADLINE capability that was IMPOSSIBLE before #7:
// register BOTH the old and the new pubkey for ONE identity at the receiver
// (`weave key add alice <old>` AND `weave key add alice <new>`), and a signed
// cross-store intent verifies whether it was signed by the OLD or the NEW key —
// true overlap with NO config trickery. Pre-#7 the second `key add` OVERWROTE the
// first, so only the most-recently-added key could verify. Then revoke the OLD
// fingerprint and prove the OLD-key message is REJECTED (R1) while the NEW key's
// still commits — all against the SAME multi-key receiver store. Mirrors the
// `signed_cross_store_send_is_verified_then_committed` idiom (real key files,
// per-actor isolated XDG_CONFIG_HOME, hermetic, no network).
// ---------------------------------------------------------------------------

/// Multi-key registry overlap E2E: receiver B registers BOTH alice's old and new
/// pubkeys under the single identity "alice"; a cross-store intent signed by the
/// OLD key COMMITS and one signed by the NEW key COMMITS (both keys verify for one
/// identity — the #7 headline). Revoking the OLD fingerprint then REJECTS the
/// OLD-key intent while the NEW-key intent still commits. Strict mode throughout,
/// so a commit proves cryptographic verification (never advisory acceptance).
#[cfg(feature = "sign")]
#[test]
fn multikey_registry_old_and_new_both_verify_then_revoke_old_through_binary() {
    // Two signing actors share the identity "alice" but hold DISTINCT key files —
    // they model alice's key BEFORE and AFTER rotation.
    let old_store = TestDb::new();
    let new_store = TestDb::new();
    let b = TestDb::new(); // ONE receiver, ONE identity, TWO registered keys.
    let old_cfg = sign_config_home_it();
    let new_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let old_cfg_s = old_cfg.to_string_lossy().into_owned();
    let new_cfg_s = new_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // alice's OLD and NEW keypairs (real key files on disk).
    let old_pub = pubkey_from_gen(&run_ok_env(
        &old_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    ));
    let new_pub = pubkey_from_gen(&run_ok_env(
        &new_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    ));
    assert_ne!(old_pub, new_pub, "rotation produces a distinct key");

    // B registers BOTH keys under the SAME identity. Pre-#7 the second add would
    // OVERWRITE the first; with #7 both are retained (rotation overlap window).
    run_ok_env(
        &b,
        &["key", "add", "alice", &old_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &b,
        &["key", "add", "alice", &new_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    // `key list` shows BOTH keys for the one identity (proof the registry kept both).
    let listing = run_ok_env(
        &b,
        &["key", "list", "--json"],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let lv: serde_json::Value = serde_json::from_str(&listing).expect("key list --json parses");
    let alice_keys: Vec<&str> = lv["keys"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["identity"] == "alice")
        .map(|e| e["pubkey"].as_str().unwrap())
        .collect();
    assert_eq!(
        alice_keys.len(),
        2,
        "both old and new keys registered for ONE identity (#7 registry): {listing}"
    );
    assert!(alice_keys.contains(&old_pub.as_str()) && alice_keys.contains(&new_pub.as_str()));

    // Resolve the OLD fingerprint (full digest) for the revoke step.
    let fp_helper = TestDb::new();
    let fph_cfg = sign_config_home_it();
    let fph_cfg_s = fph_cfg.to_string_lossy().into_owned();
    let old_full = full_fp_of_registered(&fp_helper, &fph_cfg_s, &old_pub);

    // Helper: A signs a cross-store intent for bob into B's PULL SOURCE store.
    let send_signed = |src: &TestDb, cfg_s: &str, body: &str| {
        run_ok_env(
            src,
            &[
                "send",
                "--from",
                "alice",
                "--to",
                "bob",
                "--body",
                body,
                "--to-store",
                &b.path_str(),
            ],
            &[("XDG_CONFIG_HOME", cfg_s)],
        );
    };

    // --- OVERLAP: BOTH keys verify against the multi-key receiver (STRICT) ---
    send_signed(&old_store, &old_cfg_s, "via-old-key");
    let p_old = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &old_store.path_str()),
            ("WEAVE_STRICT_VERIFY", "1"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_old.contains("pulled 1 message"),
        "the OLD key verifies against the multi-key registry under strict: {p_old}"
    );

    send_signed(&new_store, &new_cfg_s, "via-new-key");
    let p_new = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &new_store.path_str()),
            ("WEAVE_STRICT_VERIFY", "1"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_new.contains("pulled 1 message"),
        "the NEW key ALSO verifies against the SAME multi-key registry: {p_new}"
    );

    // Both committed messages are in B's inbox, both attributed to alice.
    let inbox = run_ok_env(
        &b,
        &["inbox", "--me", "bob", "--json", "--peek"],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let iv: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    let bodies: Vec<&str> = iv["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert!(
        bodies.contains(&"via-old-key") && bodies.contains(&"via-new-key"),
        "both old- and new-key signed messages committed (overlap): {inbox}"
    );

    // --- REVOKE the OLD fp: old-key intent REJECTED, new-key intent still COMMITS ---
    let b2 = TestDb::new(); // fresh receiver, same dual-key registration
    run_ok_env(
        &b2,
        &["key", "add", "alice", &old_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &b2,
        &["key", "add", "alice", &new_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let new_full = full_fp_of_registered(&fp_helper, &fph_cfg_s, &new_pub);

    let old_src2 = TestDb::new();
    run_ok_env(
        &old_src2,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "old-after-revoke",
            "--to-store",
            &b2.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    );
    let p_revoked = run_ok_env(
        &b2,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &old_src2.path_str()),
            ("WEAVE_TRUST", &new_full),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_revoked.contains("pulled 0 message"),
        "the OLD key in a multi-key set is REJECTED once its fp is revoked (R1), \
         even though it cryptographically verifies: {p_revoked}"
    );

    let new_src2 = TestDb::new();
    run_ok_env(
        &new_src2,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "new-after-revoke",
            "--to-store",
            &b2.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    );
    let p_new2 = run_ok_env(
        &b2,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &new_src2.path_str()),
            ("WEAVE_TRUST", &new_full),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_new2.contains("pulled 1 message"),
        "the NEW (non-revoked) registered key still commits after the OLD is revoked: {p_new2}"
    );

    let _ = std::fs::remove_dir_all(&old_cfg);
    let _ = std::fs::remove_dir_all(&new_cfg);
    let _ = std::fs::remove_dir_all(&fph_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// `weave key add` APPENDS (#7): adding a SECOND key for an identity keeps the
/// first; `key list` then shows BOTH, and `key remove` prunes exactly one, leaving
/// the survivor. Proves the registry is genuinely multi-key through the CLI seam.
#[cfg(feature = "sign")]
#[test]
fn key_add_appends_and_remove_prunes_one_through_binary() {
    let gen_a = TestDb::new();
    let gen_b = TestDb::new();
    let ga_cfg = sign_config_home_it();
    let gb_cfg = sign_config_home_it();
    let b = TestDb::new();
    let b_cfg = sign_config_home_it();
    let ga_cfg_s = ga_cfg.to_string_lossy().into_owned();
    let gb_cfg_s = gb_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let k1 = pubkey_from_gen(&run_ok_env(
        &gen_a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &ga_cfg_s)],
    ));
    let k2 = pubkey_from_gen(&run_ok_env(
        &gen_b,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &gb_cfg_s)],
    ));
    assert_ne!(k1, k2);

    run_ok_env(
        &b,
        &["key", "add", "alice", &k1],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &b,
        &["key", "add", "alice", &k2],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );

    let count_alice = |db: &TestDb| -> usize {
        let out = run_ok_env(
            db,
            &["key", "list", "--json"],
            &[("XDG_CONFIG_HOME", &b_cfg_s)],
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["identity"] == "alice")
            .count()
    };
    assert_eq!(count_alice(&b), 2, "key add APPENDS — both keys present");

    // Remove exactly one (by full pubkey hex). The other survives.
    let rm = run_ok_env(
        &b,
        &["key", "remove", "alice", &k1],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    assert!(rm.contains("removed"), "remove reports success: {rm}");
    assert_eq!(
        count_alice(&b),
        1,
        "exactly one key pruned, survivor remains"
    );

    let _ = std::fs::remove_dir_all(&ga_cfg);
    let _ = std::fs::remove_dir_all(&gb_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

// ---------------------------------------------------------------------------
// #11 observed-revocation audit log (A) + doctor/MCP verify-summary (B/C).
// All `--features sign`, hermetic (scrubbed env, temp WEAVE_DB, isolated
// XDG_CONFIG_HOME), driving the COMPILED binary through the CLI / MCP seams.
// ---------------------------------------------------------------------------

/// `weave audit revocations` on a fresh store: human says "0 revocation event(s)"
/// and `--json` is well-formed with `count:0` and an empty array.
#[cfg(feature = "sign")]
#[test]
fn audit_revocations_empty_store_human_and_json() {
    let db = TestDb::new();
    let human = run_ok(&db, &["audit", "revocations"]);
    assert!(
        human.contains("0 revocation event(s)"),
        "empty audit human output: {human}"
    );
    let json = run_ok(&db, &["audit", "revocations", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("audit --json parses");
    assert_eq!(v["count"], 0, "empty store: count 0");
    assert!(
        v["revocations"].as_array().unwrap().is_empty(),
        "empty store: empty array"
    );
}

/// `weave key revoke <pubkey>` records a `declared` audit event that
/// `weave audit revocations` then surfaces (fp == the normalized full digest the
/// command echoes), secret-free, in both human and `--json` form.
#[cfg(feature = "sign")]
#[test]
fn key_revoke_records_declared_event_visible_in_audit() {
    let gen = TestDb::new();
    let cfg = sign_config_home_it();
    let cfg_s = cfg.to_string_lossy().into_owned();

    let pubkey = pubkey_from_gen(&run_ok_env(
        &gen,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &cfg_s)],
    ));
    let secret = std::fs::read_to_string(cfg.join("weave").join("ed25519.key")).expect("key file");
    let secret = secret.trim().to_string();

    // Revoke records a `declared` event into THIS db's audit log + echoes the full fp.
    let revoke = run_ok_env(
        &gen,
        &["key", "revoke", &pubkey],
        &[("XDG_CONFIG_HOME", &cfg_s)],
    );
    let full_fp = revoke
        .lines()
        .find_map(|l| l.trim().strip_prefix("WEAVE_REVOKED="))
        .map(|s| s.trim().to_string())
        .expect("revoke echoes the full fingerprint");

    let json = run_ok(&gen, &["audit", "revocations", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("audit --json parses");
    assert_eq!(v["count"], 1, "one declared event recorded: {json}");
    let row = &v["revocations"][0];
    assert_eq!(row["kind"], "declared");
    assert_eq!(
        row["fp"], full_fp,
        "the declared fp matches the echoed full digest"
    );
    assert_eq!(row["identity"], "", "declared event has empty identity");

    // Secret-free: the private key never appears in audit output.
    assert!(
        !json.contains(&secret),
        "the private key must never appear in `audit revocations --json`"
    );
    let human = run_ok(&gen, &["audit", "revocations"]);
    assert!(
        human.contains("[declared]"),
        "human shows the declared event: {human}"
    );
    assert!(
        !human.contains(&secret),
        "the private key must never appear in `audit revocations`"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

/// CLI `weave doctor` (sign build) surfaces the revoked-registered + revocation-event
/// breakdown, in both human and `--json`. After registering a key and declaring its
/// revocation, the count is reflected; output stays secret-free.
#[cfg(feature = "sign")]
#[test]
fn doctor_shows_revoked_and_revocation_event_breakdown() {
    let gen = TestDb::new();
    let b = TestDb::new();
    let gen_cfg = sign_config_home_it();
    let b_cfg = sign_config_home_it();
    let gen_cfg_s = gen_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let pubkey = pubkey_from_gen(&run_ok_env(
        &gen,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &gen_cfg_s)],
    ));
    // B registers alice's key, then declares it revoked (records a declared event).
    run_ok_env(
        &b,
        &["key", "add", "alice", &pubkey],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let full_fp = full_fp_of_registered(&b, &b_cfg_s, &pubkey);

    // doctor --json carries the new sign fields; with the fp revoked, the registered
    // key is counted as hit, and one declared event was logged.
    let json = run_ok_env(
        &b,
        &["doctor", "--json"],
        &[("WEAVE_REVOKED", &full_fp), ("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let v: serde_json::Value = serde_json::from_str(&json).expect("doctor --json parses");
    assert_eq!(
        v["sign_registered_keys_revoked"], 1,
        "the registered key whose fp is revoked is counted: {json}"
    );
    assert!(
        v["sign_revocation_events"].as_i64().unwrap() >= 1,
        "the declared event is reflected in the rollup: {json}"
    );

    // doctor human shows the `revoked keys:` line.
    let human = run_ok_env(
        &b,
        &["doctor"],
        &[("WEAVE_REVOKED", &full_fp), ("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    assert!(
        human.contains("revoked keys:") && human.contains("event(s) logged"),
        "doctor human surfaces the revoked/event breakdown: {human}"
    );

    let _ = std::fs::remove_dir_all(&gen_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// MCP `weave_doctor` (sign build) emits the verify-summary section (mirrors the CLI
/// human fields) as part of the tool RESULT — never an error, no panic — and is
/// SECRET-FREE. Also covers the edge path: a store with ZERO registered keys returns
/// a well-formed summary (counts 0) rather than failing.
#[cfg(feature = "sign")]
#[test]
fn mcp_weave_doctor_emits_secret_free_sign_summary() {
    // Edge path first: zero keys, no trust config — summary still well-formed.
    {
        let db = TestDb::new();
        let mut mcp = McpServer::spawn(&db);
        let (is_err, text) = mcp.call_tool("weave_doctor", serde_json::json!({}));
        assert!(
            !is_err,
            "weave_doctor must not be an error on an empty store: {text}"
        );
        assert!(
            text.contains("signed id:"),
            "the sign verify-summary section is present: {text}"
        );
        assert!(
            text.contains("revocation log:"),
            "the revocation-log rollup line is present: {text}"
        );
        assert!(
            text.contains("revocation log: 0 event"),
            "zero events on a fresh store: {text}"
        );
        assert!(
            text.contains("my fingerprint:"),
            "the own-fingerprint line is present: {text}"
        );
        mcp.shutdown();
    }

    // With a generated key + a declared revoke, the summary reflects state and stays
    // secret-free (the private key bytes must never appear in the RESULT frame).
    {
        let db = TestDb::new();
        let cfg = sign_config_home_it();
        let cfg_s = cfg.to_string_lossy().into_owned();
        let pubkey = pubkey_from_gen(&run_ok_env(
            &db,
            &["key", "gen", "--me", "alice"],
            &[("XDG_CONFIG_HOME", &cfg_s)],
        ));
        let secret =
            std::fs::read_to_string(cfg.join("weave").join("ed25519.key")).expect("key file");
        let secret = secret.trim().to_string();
        // Register a peer key + declare a revoke so registry/log counts are non-trivial.
        run_ok_env(
            &db,
            &["key", "add", "alice", &pubkey],
            &[("XDG_CONFIG_HOME", &cfg_s)],
        );
        run_ok_env(
            &db,
            &["key", "revoke", &pubkey],
            &[("XDG_CONFIG_HOME", &cfg_s)],
        );

        let mut mcp = McpServer::spawn_env(&db, &[("XDG_CONFIG_HOME", &cfg_s)]);
        let (is_err, text) = mcp.call_tool("weave_doctor", serde_json::json!({}));
        assert!(!is_err, "weave_doctor not an error: {text}");
        assert!(text.contains("signed id:"), "sign summary present: {text}");
        assert!(
            text.contains("key registry:"),
            "registry line present: {text}"
        );
        assert!(
            text.contains("revocation log:"),
            "revocation log line present: {text}"
        );
        // SECRET-FREE: the private key must never appear in the MCP RESULT frame.
        assert!(
            !text.contains(&secret),
            "the private key must never appear in the weave_doctor RESULT"
        );
        mcp.shutdown();
        let _ = std::fs::remove_dir_all(&cfg);
    }
}

/// MCP stdout discipline (sign build): with the sign verify-summary active, the
/// `weave_doctor` exchange still emits ONLY well-formed JSON-RPC frames on stdout —
/// every line parses as JSON and carries the matching response id (no stray
/// diagnostics leaked from the new summary block).
#[cfg(feature = "sign")]
#[test]
fn mcp_weave_doctor_stdout_is_pure_jsonrpc() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);
    // call_tool already asserts: a single response line that parses as JSON with the
    // matching id and no `error` (see McpServer::request). A diagnostic written to
    // stdout would either fail JSON parse or break the id match, panicking the test.
    let (is_err, text) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(
        !is_err,
        "weave_doctor result is a clean JSON-RPC frame: {text}"
    );
    assert!(
        text.contains("signed id:"),
        "summary rode the RESULT frame: {text}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// P1 tracked ask/answer/ack — black-box (CLI + MCP)
// ---------------------------------------------------------------------------

/// Pull the minted `ask_<rowid>_<nonce>` correlation id out of a CLI/MCP line.
fn extract_cid(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.trim_end_matches([':', '.', ',']))
        .find(|w| w.starts_with("ask_"))
        .unwrap_or_else(|| panic!("no correlation id in: {text:?}"))
        .to_string()
}

/// CLI end-to-end: `weave ask` -> `weave answer` -> `weave ack` across two
/// identities. The answer lands in the asker's inbox/thread ("Re:" subject), the
/// honest delivery verdict surfaces (hermetic ⇒ not-injectable/queued), and the
/// lifecycle reaches `acked`.
#[test]
fn cli_ask_answer_ack_roundtrip() {
    let db = TestDb::new();

    // a opens a tracked ask to b.
    let opened = run_ok(
        &db,
        &[
            "ask",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "lunch",
            "--body",
            "when?",
        ],
    );
    assert!(opened.contains("opened ask"), "ask line: {opened:?}");
    assert!(opened.contains("alice -> bob"), "ask line: {opened:?}");
    // Hermetic: no real mux, no registered peer ⇒ a degrade verdict, never injected.
    assert!(
        opened.contains("recipient_not_injectable") || opened.contains("queued_next_turn"),
        "honest delivery verdict must surface: {opened:?}"
    );
    let cid = extract_cid(&opened);
    assert!(cid.starts_with("ask_"));

    // The question landed in bob's inbox.
    let b_inbox = run_ok(&db, &["inbox", "--me", "bob"]);
    assert!(
        b_inbox.contains("when?"),
        "question in bob's inbox: {b_inbox:?}"
    );

    // b answers; the answer addresses back to alice.
    let answered = run_ok(
        &db,
        &["answer", "--from", "bob", "--id", &cid, "--body", "noon"],
    );
    assert!(
        answered.contains("answered ask"),
        "answer line: {answered:?}"
    );
    assert!(
        answered.contains("-> alice"),
        "answer routes back to asker: {answered:?}"
    );

    // The answer is in alice's inbox with a "Re:" subject.
    let a_inbox = run_ok(&db, &["inbox", "--me", "alice"]);
    assert!(
        a_inbox.contains("noon"),
        "answer in alice's inbox: {a_inbox:?}"
    );
    assert!(
        a_inbox.contains("Re: lunch"),
        "Re: subject inherited: {a_inbox:?}"
    );

    // b acks to close.
    let acked = run_ok(
        &db,
        &["ack", "--from", "bob", "--id", &cid, "--message", "ttyl"],
    );
    assert!(acked.contains("closed ask"), "ack line: {acked:?}");

    // ask-get shows the terminal state + answered marker.
    let got = run_ok(&db, &["ask-get", "--id", &cid]);
    assert!(got.contains("[acked]"), "ask-get shows acked: {got:?}");
    assert!(
        got.contains("(answered)"),
        "ask-get shows answered marker: {got:?}"
    );
}

/// CLI `--json` shapes for `weave asks` and `weave ask-get` are well-formed and
/// expose the lifecycle fields.
#[test]
fn cli_asks_and_ask_get_json_shapes() {
    let db = TestDb::new();
    let opened = run_ok(
        &db,
        &["ask", "--from", "alice", "--to", "bob", "--body", "q1"],
    );
    let cid = extract_cid(&opened);

    // `weave asks --json` -> { "asks": [ { id, state, asker, askee, ... } ] }
    let asks_json = run_ok(&db, &["asks", "--me", "alice", "--role", "asker", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&asks_json).expect("asks --json is valid JSON");
    let arr = v
        .get("asks")
        .and_then(|a| a.as_array())
        .expect("asks array");
    assert_eq!(arr.len(), 1);
    assert_eq!(
        arr[0].get("id").and_then(|x| x.as_str()),
        Some(cid.as_str())
    );
    assert_eq!(arr[0].get("state").and_then(|x| x.as_str()), Some("open"));
    assert_eq!(arr[0].get("asker").and_then(|x| x.as_str()), Some("alice"));
    assert_eq!(arr[0].get("askee").and_then(|x| x.as_str()), Some("bob"));

    // `weave ask-get --json` -> { "ask": { ... } }
    let get_json = run_ok(&db, &["ask-get", "--id", &cid, "--json"]);
    let g: serde_json::Value =
        serde_json::from_str(&get_json).expect("ask-get --json is valid JSON");
    let ask = g.get("ask").expect("ask object");
    assert_eq!(ask.get("id").and_then(|x| x.as_str()), Some(cid.as_str()));
    assert_eq!(ask.get("state").and_then(|x| x.as_str()), Some("open"));
    assert!(ask
        .get("answer_msg_id")
        .map(|x| x.is_null())
        .unwrap_or(true));
}

/// CLI failure paths are clean non-zero exits (never a panic): answering/acking an
/// unknown correlation id, double-ack, a wrong-owner answer, and a metachar id.
#[test]
fn cli_ask_failure_paths_are_clean() {
    let db = TestDb::new();
    let opened = run_ok(
        &db,
        &["ask", "--from", "alice", "--to", "bob", "--body", "q"],
    );
    let cid = extract_cid(&opened);

    // Unknown correlation id.
    let (ok, _o, err) = run(
        &db,
        &[
            "answer",
            "--from",
            "bob",
            "--id",
            "ask_999_1",
            "--body",
            "x",
        ],
    );
    assert!(!ok, "answer of unknown id must fail");
    assert!(
        !err.contains("panicked"),
        "must be a clean error, not a panic: {err:?}"
    );

    // Wrong owner: the asker cannot answer its own ask.
    let (ok, _o, _e) = run(
        &db,
        &["answer", "--from", "alice", "--id", &cid, "--body", "x"],
    );
    assert!(!ok, "only the askee may answer");

    // Double-ack: ack once, then again.
    run_ok(&db, &["ack", "--from", "bob", "--id", &cid]);
    let (ok, _o, err) = run(&db, &["ack", "--from", "bob", "--id", &cid]);
    assert!(!ok, "double-ack must fail");
    assert!(
        !err.contains("panicked"),
        "double-ack is a clean error: {err:?}"
    );

    // Answer of an acked thread.
    let (ok, _o, _e) = run(
        &db,
        &["answer", "--from", "bob", "--id", &cid, "--body", "late"],
    );
    assert!(!ok, "answer of an acked thread must fail");

    // A shell-metachar correlation id is rejected before any DB bind.
    let (ok, _o, err) = run(&db, &["ack", "--from", "bob", "--id", "ask;rm -rf /"]);
    assert!(!ok, "metachar id must be rejected");
    assert!(
        !err.contains("panicked"),
        "metachar id is a clean rejection: {err:?}"
    );
}

/// MCP black-box: tools/list grows by EXACTLY 5 (weave_ask/answer/ack/asks/ask_get),
/// a happy-path ask returns a correlation id + an honest verdict (NOT isError even
/// when not injectable), and the failure paths come back as clean isError results
/// (never a panic, never a silent persist). stdout stays pure JSON-RPC (call_tool
/// asserts a single parseable frame with the matching id).
#[test]
fn mcp_ask_lifecycle_and_failures() {
    let db = TestDb::new();

    // Count the tool set: the five ask tools are present and nothing was removed.
    let mut mcp = McpServer::spawn(&db);
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    for expected in [
        "weave_ask",
        "weave_answer",
        "weave_ack",
        "weave_asks",
        "weave_ask_get",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing {expected}; got {names:?}"
        );
    }
    // Exactly five tool names start with the ask family prefix set.
    let ask_family = names
        .iter()
        .filter(|n| {
            matches!(
                n.as_str(),
                "weave_ask" | "weave_answer" | "weave_ack" | "weave_asks" | "weave_ask_get"
            )
        })
        .count();
    assert_eq!(ask_family, 5, "exactly 5 ask-family tools: {names:?}");

    // Happy path: ask to an unknown peer is HONEST SUCCESS with a verdict, NOT an
    // error (degrade-to-store), exactly like weave_send/weave_connect.
    let (is_err, ask_text) = mcp.call_tool(
        "weave_ask",
        serde_json::json!({"from": "alice", "to": "bob", "subject": "s", "body": "q?"}),
    );
    assert!(
        !is_err,
        "ask to a not-injectable peer is honest success, not an error: {ask_text}"
    );
    assert!(
        ask_text.contains("Opened ask"),
        "ask result text: {ask_text:?}"
    );
    assert!(
        ask_text.contains("recipient_not_injectable") || ask_text.contains("queued_next_turn"),
        "the honest delivery verdict vocabulary must appear: {ask_text:?}"
    );
    let cid = extract_cid(&ask_text);

    // weave_answer happy path (back to the asker) with a verdict.
    let (is_err, ans_text) = mcp.call_tool(
        "weave_answer",
        serde_json::json!({"from": "bob", "correlation_id": cid, "body": "a!"}),
    );
    assert!(!is_err, "answer happy path: {ans_text}");
    assert!(
        ans_text.contains("back to 'alice'"),
        "answer routes to asker: {ans_text:?}"
    );

    // FAILURE: answer of an unknown correlation id -> isError, clean message.
    let (is_err, t) = mcp.call_tool(
        "weave_answer",
        serde_json::json!({"from": "bob", "correlation_id": "ask_404_1", "body": "x"}),
    );
    assert!(is_err, "answer of unknown id must be isError: {t}");

    // weave_ack closes; a second ack is a clean isError (double-ack).
    let (is_err, _t) = mcp.call_tool(
        "weave_ack",
        serde_json::json!({"from": "bob", "correlation_id": cid}),
    );
    assert!(!is_err, "first ack succeeds");
    let (is_err, t) = mcp.call_tool(
        "weave_ack",
        serde_json::json!({"from": "bob", "correlation_id": cid}),
    );
    assert!(is_err, "double-ack must be a clean isError: {t}");

    // FAILURE: ack of an unknown correlation id -> isError.
    let (is_err, t) = mcp.call_tool(
        "weave_ack",
        serde_json::json!({"from": "bob", "correlation_id": "ask_999_9"}),
    );
    assert!(is_err, "ack of unknown id must be isError: {t}");

    // weave_ask_get reflects the terminal state.
    let (is_err, got) = mcp.call_tool("weave_ask_get", serde_json::json!({"id": cid}));
    assert!(!is_err, "ask_get: {got}");
    assert!(got.contains("[acked]"), "ask_get shows acked: {got:?}");

    mcp.shutdown();
}

/// MCP: a queued/not-injectable delivery for an ask is NEVER an error AND its
/// result text never leaks the message body to stdout beyond the structured
/// frame (call_tool asserts a single parseable JSON-RPC frame; here we also assert
/// the verdict sentence rather than the raw body is what surfaces). This pins the
/// honest-success-with-verdict contract.
#[test]
fn mcp_ask_verdict_is_success_not_error() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);
    let (is_err, text) = mcp.call_tool(
        "weave_ask",
        serde_json::json!({"from": "alice", "to": "ghost", "body": "secret-question"}),
    );
    assert!(!is_err, "not-injectable ask is success: {text}");
    assert!(
        text.contains("not injectable") || text.contains("next turn"),
        "a degrade verdict sentence surfaces: {text:?}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// P2 ask-many / ask-many-result — black-box (CLI + MCP)
// ---------------------------------------------------------------------------

/// Pull the minted `askm_<seed>_<nonce>` parent id out of a CLI/MCP line.
fn extract_parent_id(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.trim_end_matches([':', '.', ',']))
        .find(|w| w.starts_with("askm_"))
        .unwrap_or_else(|| panic!("no ask-many parent id in: {text:?}"))
        .to_string()
}

/// CLI end-to-end: `weave ask-many --to a --to b` opens a parent + two children
/// (both pending), `ask-many-result` shows both pending, answering one child flips
/// the rollup to 1 answered + 1 pending (still `pending`), answering the second
/// reaches `complete`. The children answer through the UNCHANGED P1 `answer` path.
#[test]
fn cli_ask_many_partial_to_complete() {
    let db = TestDb::new();
    let opened = run_ok(
        &db,
        &[
            "ask-many",
            "--from",
            "alice",
            "--to",
            "bob",
            "--to",
            "carol",
            "--subject",
            "sync",
            "--body",
            "standup?",
        ],
    );
    assert!(
        opened.contains("opened ask-many"),
        "ask-many line: {opened:?}"
    );
    assert!(
        opened.contains("2 created, 0 failed"),
        "two children created: {opened:?}"
    );
    let parent = extract_parent_id(&opened);

    // Both questions landed in the askees' inboxes.
    assert!(run_ok(&db, &["inbox", "--me", "bob"]).contains("standup?"));
    assert!(run_ok(&db, &["inbox", "--me", "carol"]).contains("standup?"));

    // Result: both pending, state pending.
    let res = run_ok(&db, &["ask-many-result", "--parent-id", &parent]);
    assert!(res.contains("[pending]"), "state pending: {res:?}");
    assert!(
        res.contains("2 pending") || res.contains("0/2 answered"),
        "rollup: {res:?}"
    );

    // The result --json lists the child correlation ids; answer the bob child.
    let res_json = run_ok(&db, &["ask-many-result", "--parent-id", &parent, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&res_json).expect("result --json valid");
    let children = v["result"]["children"].as_array().expect("children array");
    assert_eq!(children.len(), 2);
    let bob_cid = children
        .iter()
        .find(|c| c["peer"] == "bob")
        .and_then(|c| c["correlation_id"].as_str())
        .expect("bob child cid")
        .to_string();

    run_ok(
        &db,
        &["answer", "--from", "bob", "--id", &bob_cid, "--body", "yes"],
    );
    let res = run_ok(&db, &["ask-many-result", "--parent-id", &parent]);
    assert!(res.contains("1/2 answered"), "one answered: {res:?}");
    assert!(res.contains("[pending]"), "still pending: {res:?}");

    // Answer carol's child too → complete.
    let carol_cid = children
        .iter()
        .find(|c| c["peer"] == "carol")
        .and_then(|c| c["correlation_id"].as_str())
        .expect("carol child cid")
        .to_string();
    run_ok(
        &db,
        &[
            "answer", "--from", "carol", "--id", &carol_cid, "--body", "yes",
        ],
    );
    let res = run_ok(&db, &["ask-many-result", "--parent-id", &parent]);
    assert!(res.contains("[complete]"), "complete: {res:?}");

    // Unknown parent id is a clean error.
    let (ok, _o, _e) = run(&db, &["ask-many-result", "--parent-id", "askm_404_1"]);
    assert!(!ok, "unknown parent id errors");
}

/// MCP black-box: tools/list gains weave_ask_many + weave_ask_many_result; a happy
/// fanout returns the parent id + per-child verdicts (HONEST success, not isError
/// even when not injectable); an unknown peer in the list is a per-child failure
/// (call still succeeds, best-effort); empty/over-cap lists are clean isError;
/// ask_many_result of an unknown/invalid parent is isError.
#[test]
fn mcp_ask_many_lifecycle_and_failures() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    for expected in ["weave_ask_many", "weave_ask_many_result"] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected}: {names:?}"
        );
    }

    // Happy fanout: bob (valid, not-injectable) + "all" (broadcast → per-child fail).
    let (is_err, text) = mcp.call_tool(
        "weave_ask_many",
        serde_json::json!({"from": "alice", "to": ["bob", "all"], "body": "ping?"}),
    );
    assert!(
        !is_err,
        "ask-many to a non-injectable peer is honest success: {text}"
    );
    assert!(text.contains("Opened ask-many"), "ask-many text: {text:?}");
    assert!(
        text.contains("1 created, 1 failed"),
        "best-effort per child: {text:?}"
    );
    let parent = extract_parent_id(&text);

    // Result aggregate: read-only, shows 1 pending + 1 failed.
    let (is_err, res) = mcp.call_tool(
        "weave_ask_many_result",
        serde_json::json!({"parent_id": parent}),
    );
    assert!(!is_err, "result: {res}");
    assert!(res.contains("1 pending"), "rollup pending: {res:?}");
    assert!(res.contains("1 failed"), "rollup failed: {res:?}");

    // FAILURE: empty list → isError.
    let (is_err, _t) = mcp.call_tool(
        "weave_ask_many",
        serde_json::json!({"from": "alice", "to": [], "body": "q"}),
    );
    assert!(is_err, "empty to list must be isError");

    // FAILURE: over-cap list → isError.
    let big: Vec<String> = (0..70).map(|i| format!("p{i}")).collect();
    let (is_err, _t) = mcp.call_tool(
        "weave_ask_many",
        serde_json::json!({"from": "alice", "to": big, "body": "q"}),
    );
    assert!(is_err, "over-cap to list must be isError");

    // FAILURE: unknown parent / invalid parent → isError.
    let (is_err, _t) = mcp.call_tool(
        "weave_ask_many_result",
        serde_json::json!({"parent_id": "askm_404_9"}),
    );
    assert!(is_err, "unknown parent must be isError");
    let (is_err, _t) = mcp.call_tool(
        "weave_ask_many_result",
        serde_json::json!({"parent_id": "ask;rm"}),
    );
    assert!(is_err, "invalid parent id must be isError");

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// P3 job board (poll-only) — CLI roundtrip + MCP failure paths
// ---------------------------------------------------------------------------

/// Full CLI lifecycle: create -> claim -> update(running, note) -> update(completed,
/// result, fenced by attempt) -> result shows the payload. Plus a cancel path.
#[test]
fn cli_job_board_roundtrip() {
    let db = TestDb::new();

    // create (capture id from --json)
    let out = run_ok(
        &db,
        &[
            "job",
            "create",
            "--title",
            "build the thing",
            "--from",
            "alice",
            "--json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("job create --json parses");
    let id = v["job"]["id"].as_str().expect("job id present").to_string();
    assert_eq!(v["job"]["state"].as_str(), Some("queued"));
    assert_eq!(v["job"]["creator"].as_str(), Some("alice"));

    // claim (capture attempt_id)
    let out = run_ok(&db, &["job", "claim", &id, "--as", "worker", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("job claim --json parses");
    assert_eq!(v["job"]["state"].as_str(), Some("running"));
    let att = v["job"]["attempt_id"]
        .as_str()
        .expect("attempt_id present")
        .to_string();

    // update running with a note (still fenced by attempt)
    run_ok(
        &db,
        &[
            "job",
            "update",
            &id,
            "--attempt",
            &att,
            "--note",
            "halfway",
            "--json",
        ],
    );

    // complete with a result, fenced by the matching attempt
    run_ok(
        &db,
        &[
            "job",
            "update",
            &id,
            "--attempt",
            &att,
            "--state",
            "completed",
            "--result-summary",
            "shipped",
            "--result",
            r#"{"ok":true}"#,
            "--json",
        ],
    );

    // result shows the terminal payload
    let out = run_ok(&db, &["job", "result", &id, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("job result --json parses");
    assert_eq!(v["result"]["ready"].as_bool(), Some(true));
    assert_eq!(v["result"]["state"].as_str(), Some("completed"));
    assert!(v["result"]["result_json"]
        .as_str()
        .unwrap()
        .contains("true"));

    // A STALE update with NO attempt on a claimed job is fenced.
    let (ok, _o, err) = run(&db, &["job", "update", &id, "--state", "running"]);
    assert!(!ok, "update without the claim token must fail");
    let _ = err;

    // cancel path: a fresh queued job cancels straight to terminal.
    let out = run_ok(
        &db,
        &[
            "job", "create", "--title", "todo", "--from", "alice", "--json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let id2 = v["job"]["id"].as_str().unwrap().to_string();
    let out = run_ok(
        &db,
        &[
            "job", "cancel", &id2, "--reason", "obsolete", "--from", "alice", "--json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["job"]["state"].as_str(), Some("cancelled"));
    assert_eq!(v["job"]["cancel_requested"].as_bool(), Some(true));
}

/// MCP happy path + every documented failure path for the job tools.
#[test]
fn mcp_job_tools_happy_and_failure_paths() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // tools/list advertises all 8 job tool names.
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    for n in [
        "weave_job_create",
        "weave_job_list",
        "weave_job_show",
        "weave_job_status",
        "weave_job_claim",
        "weave_job_update",
        "weave_job_result",
        "weave_job_cancel",
    ] {
        assert!(names.iter().any(|x| x == n), "tools/list missing {n}");
    }

    // create (happy)
    let (is_err, text) = mcp.call_tool(
        "weave_job_create",
        serde_json::json!({"creator": "alice", "title": "do work"}),
    );
    assert!(!is_err, "create should succeed: {text}");
    // The created id is embedded in the text ("Created job job_<...> ...").
    let id = text
        .split_whitespace()
        .find(|w| w.starts_with("job_"))
        .expect("created job id in text")
        .to_string();

    // status happy
    let (is_err, _t) = mcp.call_tool("weave_job_status", serde_json::json!({"job_id": id}));
    assert!(!is_err);

    // claim happy (capture attempt_id from text "attempt_id=att_<...>")
    let (is_err, text) = mcp.call_tool(
        "weave_job_claim",
        serde_json::json!({"job_id": id, "assignee": "worker"}),
    );
    assert!(!is_err, "claim should succeed: {text}");
    let att = text
        .split_whitespace()
        .find_map(|w| w.strip_prefix("attempt_id="))
        .expect("attempt_id in claim text")
        .to_string();

    // FAILURE: update with a STALE/empty token on a claimed job → stale_attempt error.
    let (is_err, text) = mcp.call_tool(
        "weave_job_update",
        serde_json::json!({"job_id": id, "state": "completed"}),
    );
    assert!(
        is_err,
        "claimed-job update without the token must be isError"
    );
    assert!(
        text.contains("stale_attempt"),
        "fenced error surfaced: {text}"
    );

    // FAILURE: unknown job → not found.
    let (is_err, _t) = mcp.call_tool(
        "weave_job_update",
        serde_json::json!({"job_id": "job_404_9", "state": "running"}),
    );
    assert!(is_err, "unknown job must be isError");

    // FAILURE: illegal transition (complete first, then try to run).
    let (is_err, _t) = mcp.call_tool(
        "weave_job_update",
        serde_json::json!({"job_id": id, "attempt_id": att, "state": "completed"}),
    );
    assert!(!is_err, "completing a claimed job succeeds");
    let (is_err, text) = mcp.call_tool(
        "weave_job_update",
        serde_json::json!({"job_id": id, "attempt_id": att, "state": "running"}),
    );
    assert!(is_err, "completed->running must be isError");
    assert!(
        text.contains("illegal transition"),
        "transition error: {text}"
    );

    // FAILURE: bad job id never reaches a bind.
    let (is_err, _t) = mcp.call_tool("weave_job_show", serde_json::json!({"job_id": "job;rm"}));
    assert!(is_err, "metachar job id must be isError");

    // result happy (now terminal)
    let (is_err, text) = mcp.call_tool("weave_job_result", serde_json::json!({"job_id": id}));
    assert!(!is_err);
    assert!(
        text.contains("completed"),
        "result shows terminal state: {text}"
    );

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// P4: circles + orchestrator role
// ---------------------------------------------------------------------------

/// Two peers register in different circles via WEAVE_CIRCLE. `weave peers`
/// defaults to the caller's circle; `--all-circles` shows both.
#[test]
fn cli_peers_circle_scoping_and_all_circles() {
    let db = TestDb::new();
    // Register two sessions in distinct circles.
    run_ok_env(
        &db,
        &["register"],
        &[("WEAVE_SESSION", "a"), ("WEAVE_CIRCLE", "alpha")],
    );
    run_ok_env(
        &db,
        &["register"],
        &[("WEAVE_SESSION", "b"), ("WEAVE_CIRCLE", "beta")],
    );

    // Caller in 'alpha' sees only 'a' by default.
    let alpha = run_ok_env(
        &db,
        &["peers"],
        &[("WEAVE_SESSION", "a"), ("WEAVE_CIRCLE", "alpha")],
    );
    assert!(
        alpha.contains("a "),
        "alpha caller should see peer a: {alpha}"
    );
    assert!(
        !alpha.contains("\nb "),
        "alpha caller must not see peer b: {alpha}"
    );

    // --all-circles shows both.
    let all = run_ok_env(
        &db,
        &["peers", "--all-circles"],
        &[("WEAVE_SESSION", "a"), ("WEAVE_CIRCLE", "alpha")],
    );
    assert!(all.contains("a "), "all-circles should include a: {all}");
    assert!(all.contains("b "), "all-circles should include b: {all}");

    // --circle beta scopes to beta only.
    let beta = run_ok_env(
        &db,
        &["peers", "--circle", "beta"],
        &[("WEAVE_SESSION", "a"), ("WEAVE_CIRCLE", "alpha")],
    );
    assert!(beta.contains("b "), "circle=beta should show b: {beta}");
    assert!(
        !beta.contains("\na "),
        "circle=beta must not show a: {beta}"
    );
}

/// `weave orchestrator claim` then `status` reports the holder present; a second
/// peer's non-force claim while the first is live is refused.
#[test]
fn cli_orchestrator_claim_status_and_refusal() {
    let db = TestDb::new();
    // Register the rows under a foreign HOSTNAME so their stored host differs from
    // the query-time this_host ⇒ liveness fails OPEN (TTL recency-online), never a
    // pid probe (a one-shot CLI register's PID is dead by the time status runs).
    // The status/claim queries DELIBERATELY omit HOSTNAME so this_host is the real
    // machine host (≠ "remote-box"), making the rows remote ⇒ TTL-judged. This is
    // the proven remote-host liveness fixture used by the scan/peers tests.
    run_ok_env(
        &db,
        &["register"],
        &[
            ("WEAVE_SESSION", "lead"),
            ("WEAVE_CIRCLE", "ring"),
            ("HOSTNAME", "remote-box"),
        ],
    );
    run_ok_env(
        &db,
        &["register"],
        &[
            ("WEAVE_SESSION", "other"),
            ("WEAVE_CIRCLE", "ring"),
            ("HOSTNAME", "remote-box"),
        ],
    );

    let claimed = run_ok_env(
        &db,
        &["orchestrator", "claim"],
        &[("WEAVE_SESSION", "lead"), ("WEAVE_CIRCLE", "ring")],
    );
    assert!(
        claimed.contains("claimed role=orchestrator"),
        "claim text: {claimed}"
    );
    assert!(claimed.contains("lead"));

    let status = run_ok_env(
        &db,
        &["orchestrator", "status", "--circle", "ring"],
        &[("WEAVE_SESSION", "lead")],
    );
    assert!(status.contains("orchestrator present"), "status: {status}");
    assert!(status.contains("lead"), "status names the holder: {status}");

    // A second peer's non-force claim is refused while lead is live.
    let refused = run_ok_env(
        &db,
        &["orchestrator", "claim"],
        &[("WEAVE_SESSION", "other"), ("WEAVE_CIRCLE", "ring")],
    );
    assert!(
        refused.contains("refused"),
        "non-force claim refused: {refused}"
    );
    assert!(refused.contains("lead"), "names the live holder: {refused}");

    // An empty circle reports absent.
    let absent = run_ok_env(
        &db,
        &["orchestrator", "status", "--circle", "empty"],
        &[("WEAVE_SESSION", "lead")],
    );
    assert!(absent.contains("no live orchestrator"), "absent: {absent}");
}

/// REGRESSION (backward-compat): with everyone in the default circle and NO new
/// flag, `weave peers` human output is byte-identical whether or not P4 columns
/// exist — i.e. the default-circle line carries no circle/role token.
#[test]
fn cli_peers_default_circle_human_output_unchanged() {
    let db = TestDb::new();
    run_ok_env(&db, &["register"], &[("WEAVE_SESSION", "solo")]);
    let out = run_ok_env(&db, &["peers"], &[("WEAVE_SESSION", "solo")]);
    // The default-circle human line must NOT print a circle= or role= token.
    assert!(
        !out.contains("circle="),
        "default human output must not show circle=: {out}"
    );
    assert!(
        !out.contains("role="),
        "default human output must not show role=: {out}"
    );
    assert!(out.contains("solo"), "lists the peer: {out}");
}

/// MCP: weave_claim_orchestrator happy path + refusal failure path;
/// weave_orchestrator_status absent; weave_whoami echoes circle + role.
#[test]
fn mcp_orchestrator_claim_status_whoami() {
    let db = TestDb::new();
    // Register two peers in the same circle through the CLI under a foreign
    // HOSTNAME so their stored host differs from the MCP server's this_host ⇒
    // liveness fails OPEN (TTL recency-online), not pid-probed (a one-shot CLI
    // register's PID is dead). The MCP server is spawned WITHOUT HOSTNAME so its
    // this_host is the real machine host (≠ "remote-box").
    run_ok_env(
        &db,
        &["register"],
        &[
            ("WEAVE_SESSION", "lead"),
            ("WEAVE_CIRCLE", "mcpc"),
            ("HOSTNAME", "remote-box"),
        ],
    );
    run_ok_env(
        &db,
        &["register"],
        &[
            ("WEAVE_SESSION", "other"),
            ("WEAVE_CIRCLE", "mcpc"),
            ("HOSTNAME", "remote-box"),
        ],
    );

    let mut mcp = McpServer::spawn_env(&db, &[("WEAVE_SESSION", "lead"), ("WEAVE_CIRCLE", "mcpc")]);
    let _ = mcp.request("initialize", serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"it","version":"0"}}));

    // tools/list contains the two new tools.
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed
        .get("tools")
        .and_then(|t| t.as_array())
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    for expected in ["weave_claim_orchestrator", "weave_orchestrator_status"] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected}: {names:?}"
        );
    }

    // status of the circle is absent before a claim.
    let (is_err, absent) = mcp.call_tool(
        "weave_orchestrator_status",
        serde_json::json!({"circle":"mcpc"}),
    );
    assert!(!is_err);
    assert!(absent.contains("no live orchestrator"), "absent: {absent}");

    // lead claims.
    let (is_err, claimed) = mcp.call_tool(
        "weave_claim_orchestrator",
        serde_json::json!({"from":"lead"}),
    );
    assert!(!is_err, "claim not error: {claimed}");
    assert!(
        claimed.contains("claimed role=orchestrator"),
        "claim: {claimed}"
    );

    // status now present.
    let (_e, present) = mcp.call_tool(
        "weave_orchestrator_status",
        serde_json::json!({"circle":"mcpc"}),
    );
    assert!(
        present.contains("orchestrator present"),
        "present: {present}"
    );

    // FAILURE PATH: other claims without force while lead is live ⇒ refused (a
    // normal tool result, not a protocol error).
    let (is_err, refused) = mcp.call_tool(
        "weave_claim_orchestrator",
        serde_json::json!({"from":"other"}),
    );
    assert!(
        !is_err,
        "refusal is a normal result, not a protocol error: {refused}"
    );
    assert!(refused.contains("refused"), "refusal text: {refused}");

    // whoami echoes circle + role for lead.
    let (_e, who) = mcp.call_tool("weave_whoami", serde_json::json!({"me":"lead"}));
    assert!(who.contains("circle:"), "whoami has circle: {who}");
    assert!(who.contains("role:"), "whoami has role: {who}");
    assert!(
        who.contains("orchestrator"),
        "whoami shows orchestrator role: {who}"
    );

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// P6 — notify_peer + delivery observability
// ---------------------------------------------------------------------------

/// Extract the leading `#<n>` message id from a notify/send result line.
fn extract_mid(text: &str) -> i64 {
    text.split_whitespace()
        .find_map(|w| {
            let w = w.trim_start_matches('(').trim_start_matches('#');
            let w = w.trim_end_matches([',', '.', ')', ':']);
            w.parse::<i64>().ok()
        })
        .unwrap_or_else(|| panic!("no message id in: {text:?}"))
}

/// CLI end-to-end: `weave notify` returns the honest verdict, persists a normal
/// message (visible in the recipient's inbox), and `weave delivery` shows the
/// queued + not_injectable stages (hermetic ⇒ no live pane). `--json` shape checked.
#[test]
fn cli_notify_and_delivery_trace() {
    let db = TestDb::new();

    let out = run_ok(
        &db,
        &["notify", "--from", "a", "--to", "b", "--body", "ping-body"],
    );
    assert!(out.contains("notified 'b'"), "notify line: {out}");
    assert!(
        out.contains("recipient_not_injectable") || out.contains("queued_next_turn"),
        "honest verdict surfaces: {out}"
    );
    let mid = extract_mid(&out);

    // The message persisted as a normal inbox row (notify == send + verdict).
    let inbox = run_ok(&db, &["inbox", "--me", "b", "--peek"]);
    assert!(inbox.contains("ping-body"), "notify persisted: {inbox}");

    // Delivery trace shows queued + not_injectable (no live pane in a hermetic env).
    let trace = run_ok(&db, &["delivery", "--id", &mid.to_string()]);
    assert!(trace.contains("delivery trace"), "trace header: {trace}");
    assert!(trace.contains("queued"), "queued stage present: {trace}");
    assert!(
        trace.contains("not_injectable"),
        "not_injectable stage present: {trace}"
    );

    // --json shape: an array of {ts, stage, outcome, to_peer, ref_kind}.
    let json = run_ok(&db, &["delivery", "--id", &mid.to_string(), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("delivery --json parses");
    let arr = v.get("delivery").and_then(|d| d.as_array()).expect("array");
    assert!(!arr.is_empty(), "json trace non-empty: {json}");
    let first = &arr[0];
    for key in ["ts", "stage", "outcome", "to_peer", "ref_kind"] {
        assert!(first.get(key).is_some(), "json row missing {key}: {json}");
    }
    assert_eq!(
        first.get("ref_kind").and_then(|k| k.as_str()),
        Some("notify"),
        "ref_kind is notify: {json}"
    );

    // Unknown id is the empty-trace line, NOT an error.
    let (ok, empty, _e) = run(&db, &["delivery", "--id", "999999"]);
    assert!(ok, "delivery of unknown id exits 0");
    assert!(
        empty.contains("no delivery trace"),
        "empty-trace line: {empty}"
    );
}

/// CLI: a `prompt` drain records a `drained` stage for the delivered message — the
/// transport-side proof it landed in a turn. A `stop` peek does NOT drain.
#[test]
fn cli_drain_records_drained_stage() {
    let db = TestDb::new();
    run_hook(&db, "session", r#"{"cwd":"/proj/p6drain"}"#);

    let sent = run_ok(
        &db,
        &[
            "send", "--from", "bob", "--to", "p6drain", "--body", "drain-me",
        ],
    );
    let mid = extract_mid(&sent);

    // A Stop peek must NOT record a drained stage.
    run_hook(&db, "stop", r#"{"cwd":"/proj/p6drain"}"#);
    let after_stop = run_ok(&db, &["delivery", "--id", &mid.to_string()]);
    assert!(
        !after_stop.contains("drained"),
        "stop peek does not drain: {after_stop}"
    );

    // A prompt drain (explicit cwd ⇒ marks read) records the drained stage.
    run_hook(&db, "prompt", r#"{"cwd":"/proj/p6drain"}"#);
    let after_prompt = run_ok(&db, &["delivery", "--id", &mid.to_string()]);
    assert!(
        after_prompt.contains("drained"),
        "prompt drain records drained: {after_prompt}"
    );
}

/// REGRESSION: `weave send` + `weave receipts` output and read-marking are
/// unchanged with the P6 trace present (the trace is purely additive). Pins the
/// historical send/receipts contract byte-for-byte at the observable level.
#[test]
fn cli_send_and_receipts_unchanged_with_trace() {
    let db = TestDb::new();
    let sent = run_ok(
        &db,
        &["send", "--from", "x", "--to", "y", "--body", "regress"],
    );
    assert!(sent.contains("sent #"), "send line unchanged: {sent}");
    assert!(sent.contains("x -> y"), "send routing unchanged: {sent}");
    let mid = extract_mid(&sent);

    // Receipts: none until y reads it.
    let r0 = run_ok(&db, &["receipts", "--id", &mid.to_string()]);
    assert!(r0.contains("no reads yet"), "no receipts yet: {r0}");

    // y reads (marks read).
    let inbox = run_ok(&db, &["inbox", "--me", "y"]);
    assert!(inbox.contains("regress"), "y reads message: {inbox}");

    // Now receipts show y.
    let r1 = run_ok(&db, &["receipts", "--id", &mid.to_string()]);
    assert!(r1.contains("read by"), "receipts show reader: {r1}");
    assert!(r1.contains("y at"), "receipts name reader: {r1}");
}

/// BEST-EFFORT NEVER SINKS DELIVERY: a `weave notify` to a peer whose live nudge
/// FAILS (the fake mux's `send-keys` exits non-zero, the realistic "delivery path
/// errored" case) must STILL succeed — exit 0, the message persists to the inbox,
/// and the verdict prints. The trace records the `inject_failed/fail` stage,
/// proving (a) the failing inject path was exercised and (b) neither the inject
/// failure NOR the subsequent best-effort trace write sinks the notify. The message
/// is the durable contract; the nudge + trace are advisory.
#[test]
fn notify_inject_failure_never_sinks_delivery() {
    let db = TestDb::new();

    // A fake tmux: liveness probe succeeds (target considered alive) but send-keys
    // exits 1 — forcing inject() to return Err -> the InjectFailed/Fail trace stage.
    let log = common::unique_db().with_extension("tmuxlog");
    let _ = std::fs::remove_file(&log);
    let dir = common::unique_db().with_extension("muxbin");
    std::fs::create_dir_all(&dir).expect("create fake-mux dir");
    let script = dir.join("tmux");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nfor a in \"$@\"; do\n  if [ \"$a\" = send-keys ]; then exit 1; fi\ndone\nexit 0\n",
        log.display()
    );
    std::fs::write(&script, body).expect("write fake tmux");
    let mut perms = std::fs::metadata(&script).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");

    // Register 'b' as an injectable tmux pane in the SAME store (local notify).
    let reg = weave_with_fake_path(
        &db,
        &dir,
        &[("TMUX_PANE", "%8")],
        &["register", "--name", "b"],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn register");
    assert!(
        reg.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    // Notify 'b' with the failing mux on PATH. Despite the inject failing, the notify
    // MUST exit 0 and print the verdict line.
    let out = weave_with_fake_path(
        &db,
        &dir,
        &[],
        &[
            "notify",
            "--from",
            "a",
            "--to",
            "b",
            "--body",
            "survive-fail",
        ],
    )
    .stdin(Stdio::null())
    .output()
    .expect("spawn notify");
    assert!(
        out.status.success(),
        "a failed inject (and any trace write) must NOT sink the notify (exit 0): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("notified 'b'"), "notify printed: {stdout}");
    let mid = extract_mid(&stdout);

    // Confirm the inject WAS attempted and failed (we exercised the failure path).
    let logged = read_log_with_retries(&log);
    assert!(
        logged.contains("send-keys"),
        "the inject was attempted (and failed): {logged}"
    );

    // The message still persisted to b's inbox (the durable contract).
    let inbox = run_ok(&db, &["inbox", "--me", "b", "--peek"]);
    assert!(
        inbox.contains("survive-fail"),
        "message persisted despite inject failure: {inbox}"
    );

    // The trace records the failing stage — proving the post-inject best-effort write
    // happened AND captured inject_failed/fail without sinking delivery.
    let trace = run_ok(&db, &["delivery", "--id", &mid.to_string()]);
    assert!(trace.contains("queued"), "queued stage present: {trace}");
    assert!(
        trace.contains("inject_failed"),
        "inject_failed stage recorded (best-effort trace survived): {trace}"
    );
    assert!(trace.contains("fail"), "fail outcome recorded: {trace}");
}

/// MCP black-box: `weave_notify` + `weave_delivery` are present; the happy path
/// returns a verdict token (NOT isError) even for an unknown peer; broadcast and
/// oversized body are clean isError results; `weave_delivery` of a known msg lists
/// stages and of an unknown ref is the empty-trace line (not an error).
#[test]
fn mcp_notify_and_delivery_lifecycle_and_failures() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // tools/list contains both new tools.
    let listed = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = listed
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    for expected in ["weave_notify", "weave_delivery"] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing {expected}; got {names:?}"
        );
    }

    // Happy path: notify to an unknown peer is HONEST SUCCESS with a verdict, NOT an
    // error (degrade-to-store), and persists the message.
    let (is_err, text) = mcp.call_tool(
        "weave_notify",
        serde_json::json!({"from": "alice", "to": "bob", "body": "notify-secret-marker"}),
    );
    assert!(!is_err, "notify to not-injectable peer is success: {text}");
    assert!(text.contains("Notified 'bob'"), "notify result: {text:?}");
    assert!(
        text.contains("recipient_not_injectable"),
        "honest verdict token surfaces: {text:?}"
    );
    let mid = extract_mid(&text);

    // weave_delivery of the known message lists stages (queued + not_injectable).
    let (is_err, dtext) = mcp.call_tool("weave_delivery", serde_json::json!({"message_id": mid}));
    assert!(!is_err, "delivery read is not an error: {dtext}");
    assert!(dtext.contains("queued"), "queued stage: {dtext:?}");
    assert!(
        dtext.contains("not_injectable"),
        "not_injectable stage: {dtext:?}"
    );
    // SECRET-FREE: the body marker never appears in the trace surface.
    assert!(
        !dtext.contains("notify-secret-marker"),
        "delivery trace must NOT leak the body: {dtext:?}"
    );

    // FAILURE: broadcast notify -> isError pointing to send.
    let (is_err, t) = mcp.call_tool(
        "weave_notify",
        serde_json::json!({"from": "alice", "to": "all", "body": "x"}),
    );
    assert!(is_err, "broadcast notify must be isError: {t}");
    assert!(t.contains("weave_send"), "points to send: {t:?}");

    // FAILURE: oversized body -> isError, no panic / partial persist.
    let big = "z".repeat(70_000);
    let (is_err, t) = mcp.call_tool(
        "weave_notify",
        serde_json::json!({"from": "alice", "to": "bob", "body": big}),
    );
    assert!(is_err, "oversized notify body must be isError: {t}");

    // weave_delivery of an UNKNOWN ref -> empty trace line, NOT an error.
    let (is_err, t) = mcp.call_tool("weave_delivery", serde_json::json!({"message_id": 999999}));
    assert!(!is_err, "delivery of unknown ref is not an error: {t}");
    assert!(t.contains("No delivery trace"), "empty-trace line: {t:?}");

    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// Daemon lifecycle (v0.2)
// ---------------------------------------------------------------------------

#[test]
fn daemon_lifecycle_start_stop_status() {
    let db = TestDb::new();
    let pidfile = db.path.with_extension("pid");
    let pidfile_str = pidfile.to_string_lossy().into_owned();

    // Start the daemon with a test-scoped PID file for parallel safety.
    let out = run_ok_env(
        &db,
        &["daemon", "start"],
        &[("WEAVE_PIDFILE", &pidfile_str)],
    );
    assert!(
        out.contains("started") || out.contains("running"),
        "daemon start: {out}"
    );

    // Give the child a moment to write the pidfile and start its loop.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Status should report running.
    let status1 = run_ok_env(
        &db,
        &["daemon", "status"],
        &[("WEAVE_PIDFILE", &pidfile_str)],
    );
    assert!(
        status1.contains("running"),
        "daemon status after start: {status1}"
    );

    // Stop the daemon.
    let stop = run_ok_env(&db, &["daemon", "stop"], &[("WEAVE_PIDFILE", &pidfile_str)]);
    assert!(stop.contains("stopped"), "daemon stop: {stop}");

    // Status should now report stopped.
    let status2 = run_ok_env(
        &db,
        &["daemon", "status"],
        &[("WEAVE_PIDFILE", &pidfile_str)],
    );
    assert!(
        status2.contains("stopped"),
        "daemon status after stop: {status2}"
    );
}
