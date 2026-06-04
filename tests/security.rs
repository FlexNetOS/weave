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

use common::{run_env, run_ok, run_ok_env, McpServer, TestDb};
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

// ---------------------------------------------------------------------------
// 5. Attach own-row-only — a session can never overwrite ANOTHER peer's row.
// ---------------------------------------------------------------------------

/// Read the `(mux, target)` of a named peer out of `peers --json`, or `None` if
/// the peer is not present.
fn peer_mux_target(db: &TestDb, name: &str) -> Option<(String, String)> {
    let out = run_ok(db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("peers --json must parse: {e}\n{out}"));
    v.as_array()?.iter().find(|p| p["name"] == name).map(|p| {
        (
            p["mux"].as_str().unwrap_or_default().to_string(),
            p["target"].as_str().unwrap_or_default().to_string(),
        )
    })
}

/// `weave attach` binds the upserted row key to the CALLER's own resolved
/// identity (`--name`/`WEAVE_SESSION`/cwd). There is no argument that can redirect
/// the write to a foreign peer's name, so a session that attaches as "attacker"
/// can never clobber "victim"'s registered pane — even while running inside a
/// (fake) mux of its own. We prove victim's `(mux,target)` is untouched and that
/// attacker got its OWN, distinct row.
#[test]
fn attach_cannot_overwrite_another_peers_row() {
    let db = TestDb::new();

    // Victim registers from inside a tmux pane %1 (mux=tmux, target=%1).
    let (ok, _o, _e) = common::run_stdin_full(
        &db,
        &["register", "--name", "victim"],
        "",
        None,
        &[("TMUX_PANE", "%1")],
    );
    assert!(ok, "victim register failed");
    let victim_before = peer_mux_target(&db, "victim").expect("victim peer present after register");
    assert_eq!(
        victim_before,
        ("tmux".to_string(), "%1".to_string()),
        "victim registered with its own pane"
    );

    // Attacker attaches as ITSELF from a different pane %2. Even though attach
    // re-captures a live pane, the row key is "attacker", never "victim".
    let (ok, out, _e) = common::run_stdin_full(
        &db,
        &["attach", "--name", "attacker"],
        "",
        None,
        &[("TMUX_PANE", "%2")],
    );
    assert!(ok, "attacker attach must succeed: {out}");

    // Victim's row is byte-for-byte unchanged: attach wrote ONLY attacker's row.
    let victim_after = peer_mux_target(&db, "victim").expect("victim still present");
    assert_eq!(
        victim_after, victim_before,
        "attach as 'attacker' must NOT overwrite victim's (mux,target): {victim_after:?}"
    );
    // Attacker has its own, distinct row keyed by its own identity.
    let attacker = peer_mux_target(&db, "attacker").expect("attacker peer present");
    assert_eq!(
        attacker,
        ("tmux".to_string(), "%2".to_string()),
        "attacker got its own row, not victim's"
    );
}

/// An attach with a hostile / oversized identity is capped/rejected, never
/// persisted. Over `MAX_IDENT_LEN` the store's `check_ident` refuses it (exit
/// non-zero, nothing written), so a giant name can't bloat the peer table.
#[test]
fn attach_oversized_identity_is_rejected() {
    let db = TestDb::new();
    let giant = "x".repeat(100_000);
    let (ok, _o, err) = common::run(&db, &["attach", "--name", &giant]);
    assert!(!ok, "an oversized attach identity must be rejected");
    assert!(
        err.to_lowercase().contains("too long") || err.to_lowercase().contains("identifier"),
        "clear identity-cap error: {err}"
    );
    // Nothing was persisted under the giant name.
    let peers = run_ok(&db, &["peers", "--json"]);
    assert!(
        !peers.contains(&giant),
        "a rejected oversized attach must not persist a peer row"
    );
}

/// Read a peer's persisted `host` field from `peers --json`.
fn peer_host(db: &TestDb, name: &str) -> Option<String> {
    let out = run_ok(db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("peers --json must parse: {e}\n{out}"));
    v.as_array()?
        .iter()
        .find(|p| p["name"] == name)
        .map(|p| p["host"].as_str().unwrap_or_default().to_string())
}

/// A2: the per-peer `host` label is derived from `$HOSTNAME` (`config::this_host`)
/// and must be length-bounded (`MAX_HOST_LEN` = 128) and control-char-free before
/// it is persisted. A hostile/oversized `$HOSTNAME` at registration time must NOT
/// land an unbounded or control-bearing value into the peer row — `register`
/// succeeds (the host is sanitized, not rejected) but the stored value is capped
/// and control-free. This proves `this_host()` cannot inject an unbounded or
/// control-laden string into the store, even though it is also bound as a SQL
/// parameter (never a literal).
#[test]
fn hostile_oversized_hostname_is_capped_and_control_stripped() {
    const MAX_HOST_LEN: usize = 128;
    let db = TestDb::new();

    // An oversized HOSTNAME carrying embedded control characters (newline, tab,
    // carriage return, an escape, a bell) plus a long ASCII run. (NUL is excluded
    // because the OS forbids NUL bytes in an env var value, so it can never reach
    // `this_host()` via `$HOSTNAME` in the first place.)
    let hostile = format!("evil\n\t\r\u{1b}\u{7}{}", "Z".repeat(5_000));
    let (ok, _o, err) = common::run_stdin_full(
        &db,
        &["register", "--name", "hh"],
        "",
        None,
        &[("HOSTNAME", &hostile)],
    );
    assert!(ok, "register must succeed with a sanitized host: {err}");

    let host = peer_host(&db, "hh").expect("hh peer present after register");
    // Bounded.
    assert!(
        host.chars().count() <= MAX_HOST_LEN,
        "stored host must be <= MAX_HOST_LEN chars, got {} chars: {host:?}",
        host.chars().count()
    );
    // Control-char free.
    assert!(
        !host.chars().any(|c| c.is_control()),
        "stored host must contain no control characters: {host:?}"
    );
    // Non-empty (sanitizing did not empty it, so no spurious fallback needed here).
    assert!(!host.is_empty(), "stored host must not be empty: {host:?}");
}

/// A2: when `$HOSTNAME` is present but sanitizes to empty (only control chars /
/// whitespace), `this_host()` falls back to the stable `"local"` label rather
/// than persisting an empty/garbage host. Confirms the derived host is always a
/// safe, bounded, non-empty identity.
#[test]
fn control_only_hostname_falls_back_to_local() {
    let db = TestDb::new();
    // HOSTNAME made entirely of (env-deliverable) control characters -> sanitizes
    // to empty -> "local". (NUL is not used: the OS forbids it in an env value.)
    let (ok, _o, err) = common::run_stdin_full(
        &db,
        &["register", "--name", "cc"],
        "",
        None,
        &[("HOSTNAME", "\u{1}\u{7}\u{1b}\t")],
    );
    assert!(ok, "register must succeed: {err}");
    let host = peer_host(&db, "cc").expect("cc peer present");
    assert!(
        !host.chars().any(|c| c.is_control()),
        "host is control-free: {host:?}"
    );
    assert_eq!(
        host, "local",
        "a control-only HOSTNAME must fall back to the 'local' label, got {host:?}"
    );
}

// ---------------------------------------------------------------------------
// Tier-1 federation read-only guarantee — the headline security property.
// A configured foreign store is read but NEVER written: no migration, no
// journal/WAL write, no row touched. The structural proof at the binary level.
// ---------------------------------------------------------------------------

/// Snapshot a foreign store's exact DATA bytes, run federated `peers`, `sessions`
/// AND `doctor` against it, and assert the data store is unchanged: the main DB
/// file is byte-identical (no row touched, no migration) and the `-wal` carries
/// NO committed write (stays absent or 0 bytes). This is the binary-level proof
/// of the read-only invariant — a federated read cannot mutate a store it does
/// not own.
///
/// Note on `-shm`: a WAL-mode SQLite database REQUIRES the shared-memory index
/// (`-shm`) to coordinate readers, so a *read-only* open legitimately materializes
/// an empty `-shm` (this is documented SQLite behavior and is not a data write).
/// The invariant we assert is therefore on the data files — the main DB content
/// and the (empty) write-ahead log — not on the reader-coordination `-shm`.
#[test]
fn federation_never_writes_the_foreign_store() {
    let local = TestDb::new();
    let foreign = TestDb::new();

    // Seed the foreign store with a peer and an unread session, then let the
    // writer process exit so all its WAL/journal is flushed/checkpointed.
    run_ok(&foreign, &["register", "--name", "guest"]);
    run_ok(
        &foreign,
        &["send", "--from", "a", "--to", "guest", "--body", "hello"],
    );
    // A no-op read to settle any sidecar state before snapshotting.
    let _ = run_ok(&foreign, &["peers"]);

    let base = foreign.path_str();
    let main_path = base.clone();
    let wal_path = format!("{base}-wal");
    let journal_path = format!("{base}-journal");

    // The authoritative data store: the main DB file must be byte-identical.
    let main_before = std::fs::read(&main_path).expect("read foreign main DB (before)");

    // Federated reads against the foreign store, through the real binary.
    let env = [("WEAVE_PEER_DBS", base.as_str())];
    let peers = run_ok_env(&local, &["peers", "--json"], &env);
    assert!(
        peers.contains("guest"),
        "federated read must actually see the foreign peer: {peers}"
    );
    let _ = run_ok_env(&local, &["sessions", "--json"], &env);
    // doctor also opens each extra store for its federation status line.
    let _ = run_ok_env(&local, &["doctor", "--json"], &env);

    // The main DB content is byte-for-byte unchanged: no row, no schema, no write.
    let main_after = std::fs::read(&main_path).expect("read foreign main DB (after)");
    assert_eq!(
        main_before, main_after,
        "a federated read-only open must leave the foreign store's main DB file \
         byte-identical — no migration, no row write"
    );
    // No write was committed: the write-ahead log is absent or empty, and no
    // rollback journal was created.
    let wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        wal_len, 0,
        "a federated read must not commit a write (WAL must be empty/absent)"
    );
    assert!(
        std::fs::metadata(&journal_path).is_err(),
        "a federated read must not create a rollback journal on the foreign store"
    );
}

/// Path/identity cap: a hostile 1000-entry `WEAVE_PEER_DBS` does NOT open 1000
/// stores — `MAX_PEER_DBS` (16) bounds the fan-out — and the command still
/// succeeds (the local listing returns). Bounds the N+1 open amplification.
#[test]
fn federation_caps_an_oversized_peer_db_list() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    // 1000 distinct, nonexistent paths. Each would be one open attempt if
    // uncapped; the cap drops everything past 16. We assert (a) the command still
    // succeeds and lists the local peer, and (b) the cap note is emitted on
    // stderr — proving truncation happened rather than a 1000-wide fan-out.
    let list = (0..1000)
        .map(|i| format!("/tmp/weave-cap-{i}.db"))
        .collect::<Vec<_>>()
        .join(",");
    let (ok, out, err) = run_env(&local, &["peers"], &[("WEAVE_PEER_DBS", &list)]);
    assert!(ok, "an oversized list must not fail the command");
    assert!(out.contains("here"), "local peer still listed: {out}");
    assert!(
        err.contains("capping at"),
        "the cap must be diagnosed on stderr (truncation happened): {err}"
    );
}

/// A path-traversal-y / absolute junk path in `WEAVE_PEER_DBS` cannot escalate to
/// a write: it is opened read-only (or fails to open) and is never created. The
/// junk target must not exist afterward, and the command stays successful.
#[test]
fn federation_junk_path_cannot_escalate_to_a_write() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    // A traversal-style path under a temp dir that does not exist. A read-only
    // open must NOT create it (no create_dir_all, no CREATE flag).
    let junk = std::env::temp_dir().join(format!(
        "weave-trav-{}/../weave-escalate-{}.db",
        std::process::id(),
        std::process::id()
    ));
    let junk_str = junk.to_string_lossy().into_owned();
    let (ok, out, _err) = run_env(&local, &["peers"], &[("WEAVE_PEER_DBS", &junk_str)]);
    assert!(ok, "a junk federated path must not fail the command");
    assert!(out.contains("here"), "local peer still listed: {out}");
    // The resolved target was never created by the read-only open.
    let resolved = std::env::temp_dir().join(format!("weave-escalate-{}.db", std::process::id()));
    assert!(
        !resolved.exists(),
        "a read-only federated open must never create the target file"
    );
}
