//! Security / hardening end-to-end tests for the built `weave` binary.
//!
//! Like `integration.rs`, every test drives the real binary as a black box via
//! `std::process::Command` (path resolved at compile time through the shared
//! `common` helpers) and uses its own unique temp `WEAVE_DB`, so the suite stays
//! isolated, parallel-safe, and never touches the real store.
//!
//! These tests pin down properties that must hold for the tool to be safe to run
//! between untrusted-ish agent sessions:
//!
//!   1. Flag-injection resistance: a message body that *looks* like a CLI flag
//!      (begins with `--` or `-n`) is delivered byte-for-byte and is never
//!      re-interpreted as an option, neither on the way in (CLI parse) nor on the
//!      way out (inbox render).
//!   2. Destructive-op guard: the MCP `weave_clear` with `scope=all` refuses to
//!      run without an explicit `confirm`, so a stray call can't wipe the mesh.
//!   3. Resource guard: an absurdly oversized identity is rejected by the MCP
//!      layer with an `isError` result rather than being persisted.
//!   4. At-rest secrecy: the sqlite db file is created without group/other access
//!      (`mode & 0o077 == 0`), so other local users can't read message contents.

mod common;

use common::{run_ok, McpServer, TestDb};
use std::os::unix::fs::PermissionsExt;

// ---------------------------------------------------------------------------
// 1. Flag-injection resistance — bodies that look like flags are verbatim.
// ---------------------------------------------------------------------------

/// Pull the single message body out of `weave inbox --json --peek` output.
///
/// Using `--json` (and the `=`-form for the body on the way in) means we compare
/// the *exact* stored bytes, not a pretty-printed line we'd have to unwrap — so a
/// body that started with `--` or `-n` is proven to have survived end-to-end with
/// zero mangling.
fn only_body(db: &TestDb, me: &str) -> String {
    let out = run_ok(db, &["inbox", "--me", me, "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("inbox --json must parse: {e}\n{out}"));
    let msgs = v["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("inbox json missing `messages` array: {v}"));
    assert_eq!(
        msgs.len(),
        1,
        "expected exactly one message in inbox for '{me}': {v}"
    );
    msgs[0]["body"]
        .as_str()
        .unwrap_or_else(|| panic!("message body should be a string: {v}"))
        .to_string()
}

/// A body beginning with `--` must be stored and rendered verbatim. We pass it
/// via the `--body=<value>` form, the canonical way to hand clap a value that
/// starts with a dash; the assertion proves clap consumed it as data (it never
/// errored on an "unknown flag") and the store kept every byte.
#[test]
fn body_starting_with_double_dash_is_delivered_verbatim() {
    let db = TestDb::new();
    let payload = "--to=victim --body=pwned; rm -rf /";

    let sent = run_ok(
        &db,
        &[
            "send",
            "--from",
            "attacker",
            "--to",
            "bob",
            &format!("--body={payload}"),
        ],
    );
    assert!(
        sent.contains("attacker -> bob"),
        "send should confirm the route attacker -> bob: {sent:?}"
    );

    let got = only_body(&db, "bob");
    assert_eq!(
        got, payload,
        "a `--`-prefixed body must arrive byte-for-byte, never parsed as a flag"
    );
}

/// A body beginning with `-n` (a short-flag shape, and historically a token that
/// `echo`/`printf`-style parsers eat) must likewise be delivered untouched.
#[test]
fn body_starting_with_short_flag_is_delivered_verbatim() {
    let db = TestDb::new();
    let payload = "-n -e --peek not a flag\nsecond line";

    let sent = run_ok(
        &db,
        &[
            "send",
            "--from",
            "a",
            "--to",
            "b",
            &format!("--body={payload}"),
        ],
    );
    assert!(sent.contains("a -> b"), "send route confirmed: {sent:?}");

    let got = only_body(&db, "b");
    assert_eq!(
        got, payload,
        "a `-n`-prefixed body must arrive verbatim (including the newline), never parsed as a flag"
    );
}

// ---------------------------------------------------------------------------
// 2. Destructive-op guard — clear scope=all needs explicit confirm.
// ---------------------------------------------------------------------------

/// `weave_clear {scope: all}` without `confirm` must refuse: the call returns an
/// `isError` result, the warning mentions confirmation, and — crucially — the
/// pre-existing message is still readable afterward (nothing was wiped).
#[test]
fn mcp_clear_all_without_confirm_is_refused_and_preserves_data() {
    let db = TestDb::new();

    // Seed one message so we can prove the refusal left the store intact.
    run_ok(
        &db,
        &["send", "--from", "a", "--to", "b", "--body", "keepme"],
    );

    let mut mcp = McpServer::spawn(&db);

    // scope=all with no confirm => must be an error.
    let (is_err, text) = mcp.call_tool(
        "weave_clear",
        serde_json::json!({"me": "b", "scope": "all"}),
    );
    assert!(
        is_err,
        "weave_clear scope=all without confirm must be an error, got ok: {text:?}"
    );
    assert!(
        text.to_lowercase().contains("confirm"),
        "the refusal should tell the caller to confirm: {text:?}"
    );

    mcp.shutdown();

    // The seeded message must still be deliverable — the refusal wiped nothing.
    let inbox = run_ok(&db, &["inbox", "--me", "b", "--peek"]);
    assert!(
        inbox.contains("keepme"),
        "a refused clear-all must leave existing messages intact: {inbox:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Resource guard — oversized identity rejected by the MCP layer.
// ---------------------------------------------------------------------------

/// An identity (here the `from` sender) of absurd length is hostile/buggy input.
/// The MCP layer must reject it with an `isError` result rather than persisting a
/// 100k-char row. We then confirm nothing was delivered under that giant name.
#[test]
fn mcp_send_with_oversized_identity_is_rejected() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let giant = "x".repeat(100_000);
    let (is_err, text) = mcp.call_tool(
        "weave_send",
        serde_json::json!({"from": giant, "to": "bob", "body": "hi"}),
    );
    assert!(
        is_err,
        "weave_send with a 100k-char sender must be rejected (isError), got ok: {}",
        // Don't echo the whole 100k payload back into the panic message.
        &text[..text.len().min(200)]
    );

    mcp.shutdown();

    // Belt and suspenders: the oversized send must not have landed in bob's inbox.
    let inbox = run_ok(&db, &["inbox", "--me", "bob", "--peek"]);
    assert!(
        inbox.contains("empty"),
        "a rejected oversized send must not persist any message: {inbox:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. At-rest secrecy — the db file is not group/world readable.
// ---------------------------------------------------------------------------

/// After a send creates the sqlite db, its permission bits must grant no access
/// to group or other (`mode & 0o077 == 0`): message bodies can be sensitive and
/// must not leak to other local users.
#[test]
fn db_file_is_not_world_or_group_readable() {
    let db = TestDb::new();

    // Force the file into existence with a real write.
    run_ok(
        &db,
        &["send", "--from", "a", "--to", "b", "--body", "secret"],
    );

    let meta = std::fs::metadata(&db.path)
        .unwrap_or_else(|e| panic!("db file {:?} should exist after a send: {e}", db.path));
    let mode = meta.permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "db file {:?} must not be group/other accessible; mode was {:o}",
        db.path,
        mode & 0o777
    );
}

#[test]
fn oversized_body_is_rejected() {
    let db = TestDb::new();
    let big = "x".repeat(70_000); // > MAX_BODY (65536)
    let (ok, _o, err) = common::run(&db, &["send", "--from", "a", "--to", "b", "--body", &big]);
    assert!(!ok, "an oversized body must be rejected, not stored");
    assert!(err.contains("too long"), "clear error: {err}");
}
