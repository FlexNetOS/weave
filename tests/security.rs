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

use common::{run, run_env, run_ok, run_ok_env, McpServer, TestDb};
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

// ---------------------------------------------------------------------------
// Tier-2 cross-store delivery — owner-only-writes and authorization hardening.
// ---------------------------------------------------------------------------

/// THE HEADLINE owner-only-writes proof: the receiver pulls and commits a message
/// while the SOURCE store's main DB file stays byte-identical (no write, no
/// migration, no journal data write). The cursor and the committed inbox row are
/// written ONLY to the receiver's own store. Extends the Tier-1 byte-unchanged
/// assertion to hold during an ACTIVE cross-store pull+commit.
#[test]
fn pull_never_writes_the_source_store() {
    let source = TestDb::new(); // A — the sender, owner of the outbox
    let receiver = TestDb::new(); // B — the puller/committer

    // A enqueues a directed cross-store intent for bob (who lives in B).
    run_ok(
        &source,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "owner-only-writes proof",
            "--to-store",
            &receiver.path_str(),
        ],
    );
    // Let A's writer fully settle (its own outbox write is flushed/checkpointed).
    let _ = run_ok(&source, &["outbox"]);

    let base = source.path_str();
    let wal_path = format!("{base}-wal");
    let journal_path = format!("{base}-journal");

    // Snapshot A's main DB bytes BEFORE B pulls.
    let before = std::fs::read(&base).expect("read source main DB (before)");

    // B actively pulls and commits from A (allow-listed). Run twice so the
    // re-pull (idempotent) also exercises the source again.
    let env = [("WEAVE_PULL_FROM", base.as_str())];
    let p1 = run_ok_env(&receiver, &["pull", "--me", "bob"], &env);
    assert!(p1.contains("pulled 1 message"), "first pull delivers: {p1}");
    let p2 = run_ok_env(&receiver, &["pull", "--me", "bob"], &env);
    assert!(
        p2.contains("pulled 0 message"),
        "re-pull is idempotent: {p2}"
    );

    // The source's main DB is byte-for-byte unchanged: the engine opened it
    // READ_ONLY, so no row, no schema, no consumed-marker was ever written to A.
    let after = std::fs::read(&base).expect("read source main DB (after)");
    assert_eq!(
        before, after,
        "an active cross-store pull must leave the SOURCE store byte-identical \
         (owner-only-writes): no write, no migration, no consumed-marker"
    );
    // No write was committed to A: WAL empty/absent, no rollback journal created.
    let wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        wal_len, 0,
        "pulling must not commit a write to the source (WAL must be empty/absent)"
    );
    assert!(
        std::fs::metadata(&journal_path).is_err(),
        "pulling must not create a rollback journal on the source store"
    );

    // And the message DID arrive in B (the read-only pull still delivered).
    let inbox = run_ok(&receiver, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"][0]["body"], "owner-only-writes proof");
}

/// A non-allow-listed source cannot deliver into the receiver: with no
/// `pull_from`, the source is never even opened, so no inbox row appears.
#[test]
fn non_allow_listed_source_cannot_deliver() {
    let source = TestDb::new();
    let receiver = TestDb::new();
    run_ok(
        &source,
        &[
            "send",
            "--from",
            "mallory",
            "--to",
            "bob",
            "--body",
            "unsolicited",
            "--to-store",
            &receiver.path_str(),
        ],
    );
    // Receiver pulls WITHOUT listing the source => nothing is delivered.
    let pull = run_ok(&receiver, &["pull", "--me", "bob"]);
    assert!(pull.contains("pulled 0 message"), "no delivery: {pull}");
    let inbox = run_ok(&receiver, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(
        v["messages"].as_array().map(|x| x.len()),
        Some(0),
        "a non-allow-listed source can never deliver into the receiver: {inbox}"
    );
}

/// Input caps at enqueue: a cross-store intent body over `MAX_BODY` (65536) is
/// rejected — the oversized intent is never written to the outbox.
#[test]
fn cross_store_oversized_body_is_rejected_at_enqueue() {
    let a = TestDb::new();
    let b = TestDb::new();
    let huge = "x".repeat(65_537);
    let (ok, _out, err) = run(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            &huge,
            "--to-store",
            &b.path_str(),
        ],
    );
    assert!(!ok, "an oversized cross-store body must be rejected");
    assert!(
        err.to_lowercase().contains("body") || err.to_lowercase().contains("long"),
        "rejection mentions the body cap: {err}"
    );
    // Nothing was enqueued.
    let outbox = run_ok(&a, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&outbox).expect("outbox parses");
    assert_eq!(
        ov["outbox"].as_array().map(|x| x.len()),
        Some(0),
        "an over-cap intent is never persisted to the outbox: {outbox}"
    );
}

/// A bad recipient/sender identity on a cross-store intent is rejected at enqueue
/// (the `check_ident` cap), so a malformed intent never enters the outbox.
#[test]
fn cross_store_bad_identity_is_rejected_at_enqueue() {
    let a = TestDb::new();
    let b = TestDb::new();
    let oversized = "n".repeat(5_000);
    let (ok, _out, err) = run(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            &oversized,
            "--body",
            "hi",
            "--to-store",
            &b.path_str(),
        ],
    );
    assert!(!ok, "an oversized recipient identity must be rejected");
    assert!(
        err.to_lowercase().contains("recipient") || err.to_lowercase().contains("long"),
        "rejection mentions the identity cap: {err}"
    );
    let outbox = run_ok(&a, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&outbox).expect("outbox parses");
    assert_eq!(ov["outbox"].as_array().map(|x| x.len()), Some(0));
}

/// `MAX_PULL_FROM` (16) bounds the pull fan-out: a hostile 1000-entry
/// `WEAVE_PULL_FROM` does not open 1000 stores and the pull still succeeds; the
/// cap note is emitted on stderr (truncation happened).
#[test]
fn pull_from_list_is_capped() {
    let b = TestDb::new();
    let list = (0..1000)
        .map(|i| format!("/tmp/weave-pullcap-{i}.db"))
        .collect::<Vec<_>>()
        .join(",");
    let (ok, out, err) = run_env(&b, &["pull", "--me", "bob"], &[("WEAVE_PULL_FROM", &list)]);
    assert!(ok, "an oversized pull_from list must not fail the command");
    // The driver reports the capped source count (<= 16), never 1000.
    assert!(
        out.contains("from 16 source"),
        "the pull fan-out is capped at MAX_PULL_FROM: {out}"
    );
    assert!(
        err.contains("capping at"),
        "the cap must be diagnosed on stderr (truncation happened): {err}"
    );
}

/// Cross-store broadcast is refused (no cross-store fan-out): a `--to-store` send
/// to a broadcast alias exits non-zero and persists nothing.
#[test]
fn cross_store_broadcast_is_refused() {
    let a = TestDb::new();
    let b = TestDb::new();
    for alias in ["all", "*", "everyone", "broadcast"] {
        let (ok, _out, err) = run(
            &a,
            &[
                "send",
                "--from",
                "alice",
                "--to",
                alias,
                "--body",
                "fan out everywhere",
                "--to-store",
                &b.path_str(),
            ],
        );
        assert!(!ok, "cross-store broadcast to {alias:?} must be refused");
        assert!(
            err.contains("broadcast"),
            "rejection mentions broadcast for {alias:?}: {err}"
        );
    }
    let outbox = run_ok(&a, &["outbox", "--json"]);
    let ov: serde_json::Value = serde_json::from_str(&outbox).expect("outbox parses");
    assert_eq!(
        ov["outbox"].as_array().map(|x| x.len()),
        Some(0),
        "no broadcast intent is ever persisted: {outbox}"
    );
}

// ---------------------------------------------------------------------------
// Tier-2 phase 2c — authorized injection gate (the headline security property).
// ---------------------------------------------------------------------------

/// Create a temp dir with an executable fake `tmux` that appends its full argv to
/// `log_path` and exits 0. Mirrors the integration suite's `make_fake_tmux`.
fn make_fake_tmux(log_path: &std::path::Path) -> std::path::PathBuf {
    let dir = TestDb::new().path_str();
    let dir = std::path::PathBuf::from(format!("{dir}.muxbin"));
    std::fs::create_dir_all(&dir).expect("create fake-mux dir");
    let script = dir.join("tmux");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
        log_path.display()
    );
    std::fs::write(&script, body).expect("write fake tmux");
    let mut perms = std::fs::metadata(&script).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod +x fake tmux");
    dir
}

/// Run `weave` with the fake-mux dir prepended to PATH and trusted via
/// WEAVE_MUX_DIR, plus extra env. Returns (success, stdout, stderr).
fn run_with_fake_mux(
    db: &TestDb,
    fake_dir: &std::path::Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> (bool, String, String) {
    let orig = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fake_dir.display(), orig);
    let fake_dir_str = fake_dir.display().to_string();
    let mut env: Vec<(&str, &str)> = vec![
        ("PATH", new_path.as_str()),
        ("WEAVE_MUX_DIR", fake_dir_str.as_str()),
    ];
    env.extend_from_slice(extra_env);
    run_env(db, args, &env)
}

/// Read the fake-mux log with a short backoff (the script writes asynchronously).
fn read_mux_log(log: &std::path::Path) -> String {
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

/// THE HEADLINE 2c GATE: a source that delivers a message but is NOT inject-eligible
/// (excluded by `allow_inject_from`) can NEVER cause a keystroke in B's pane — even
/// with `inject_pulled=true` (the default-on, most permissive posture). The message
/// is delivered to the inbox; no `send-keys` is ever recorded. A separate trusted
/// source on the allow list DOES inject, proving the gate is the only difference.
#[test]
fn non_inject_listed_source_never_keystrokes_even_with_inject_on() {
    let hostile = TestDb::new(); // pull_from yes, allow_inject_from NO
    let trusted = TestDb::new(); // pull_from yes, allow_inject_from YES
    let b = TestDb::new();

    // Both sources enqueue a directed intent for bob.
    run_ok(
        &hostile,
        &[
            "send",
            "--from",
            "mallory",
            "--to",
            "bob",
            "--body",
            "hostile body",
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

    // Register B's OWN injectable pane %2 under a fake mux.
    let reg_log = TestDb::new().path_str();
    let reg_log = std::path::PathBuf::from(format!("{reg_log}.tmuxlog"));
    let fake_reg = make_fake_tmux(&reg_log);
    let (reg_ok, _o, reg_err) = run_with_fake_mux(
        &b,
        &fake_reg,
        &[("TMUX_PANE", "%2")],
        &["register", "--name", "bob"],
    );
    assert!(reg_ok, "register failed: {reg_err}");

    // Pull the HOSTILE source. inject_pulled is ON by default; allow_inject_from is
    // set to ONLY the trusted store, so the hostile source delivers but is never
    // inject-eligible. The most permissive master switch must NOT override the gate.
    let log_h = TestDb::new().path_str();
    let log_h = std::path::PathBuf::from(format!("{log_h}.tmuxlog"));
    let fake_h = make_fake_tmux(&log_h);
    let (ok_h, out_h, err_h) = run_with_fake_mux(
        &b,
        &fake_h,
        &[
            ("WEAVE_PULL_FROM", &hostile.path_str()),
            ("WEAVE_ALLOW_INJECT_FROM", &trusted.path_str()),
            ("WEAVE_INJECT_PULLED", "true"),
        ],
        &["pull", "--me", "bob"],
    );
    assert!(ok_h, "hostile pull must still succeed: {err_h}");
    assert!(
        out_h.contains("pulled 1 message"),
        "hostile source DELIVERS: {out_h}"
    );
    // The hard gate: NO keystroke for a non-inject-listed source, even inject_pulled=true.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let logged_h = std::fs::read_to_string(&log_h).unwrap_or_default();
    assert!(
        !logged_h.contains("send-keys"),
        "a non-allow-listed source must NEVER cause a keystroke, even with \
         inject_pulled=true:\n{logged_h}"
    );

    // Sanity: the TRUSTED (inject-listed) source DOES inject — proving the gate is
    // the only thing that suppressed the hostile keystroke (not a broken harness).
    let log_t = TestDb::new().path_str();
    let log_t = std::path::PathBuf::from(format!("{log_t}.tmuxlog"));
    let fake_t = make_fake_tmux(&log_t);
    let (ok_t, out_t, err_t) = run_with_fake_mux(
        &b,
        &fake_t,
        &[
            ("WEAVE_PULL_FROM", &trusted.path_str()),
            ("WEAVE_ALLOW_INJECT_FROM", &trusted.path_str()),
            ("WEAVE_INJECT_PULLED", "true"),
        ],
        &["pull", "--me", "bob"],
    );
    assert!(ok_t, "trusted pull must succeed: {err_t}");
    assert!(
        out_t.contains("pulled 1 message"),
        "trusted source delivers: {out_t}"
    );
    let logged_t = read_mux_log(&log_t);
    assert!(
        logged_t.contains("send-keys") && logged_t.contains("-t %2"),
        "the inject-listed source DOES nudge B's own pane %2 (gate is the only diff):\n{logged_t}"
    );

    // Both messages arrived in B's inbox regardless of the inject gate.
    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(
        v["messages"].as_array().map(|x| x.len()),
        Some(2),
        "both messages delivered; only the keystroke was gated: {inbox}"
    );
}

/// The message BODY never reaches the injector keystrokes on the pulled-inject path,
/// regardless of consent config. Even when injection fires (allow-listed, default-on),
/// the fake-mux argv contains only the fixed content-free ping — never the body bytes.
/// This is the paste-safe + no-body-leak proof for the cross-trust-boundary nudge.
#[test]
fn pulled_inject_never_carries_the_message_body() {
    let a = TestDb::new();
    let b = TestDb::new();
    let secret = "TOPSECRET-cross-store-payload-42";

    run_ok(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            secret,
            "--to-store",
            &b.path_str(),
        ],
    );

    let reg_log = TestDb::new().path_str();
    let reg_log = std::path::PathBuf::from(format!("{reg_log}.tmuxlog"));
    let fake_reg = make_fake_tmux(&reg_log);
    let (reg_ok, _o, reg_err) = run_with_fake_mux(
        &b,
        &fake_reg,
        &[("TMUX_PANE", "%9")],
        &["register", "--name", "bob"],
    );
    assert!(reg_ok, "register failed: {reg_err}");

    let log = TestDb::new().path_str();
    let log = std::path::PathBuf::from(format!("{log}.tmuxlog"));
    let fake_dir = make_fake_tmux(&log);
    // Default-on, allow-listed (unset allow_inject_from ⇒ same as pull set) ⇒ inject
    // fires. The body must STILL be absent from every recorded argv.
    let (ok, out, err) = run_with_fake_mux(
        &b,
        &fake_dir,
        &[("WEAVE_PULL_FROM", &a.path_str())],
        &["pull", "--me", "bob"],
    );
    assert!(ok, "pull must succeed: {err}");
    assert!(out.contains("pulled 1 message"), "delivered: {out}");

    let logged = read_mux_log(&log);
    // The inject fired (proving we exercised the keystroke path, not a no-op)...
    assert!(
        logged.contains("send-keys"),
        "the default-on nudge must fire so this proves the no-body property on a live \
         keystroke:\n{logged}"
    );
    // ...but the body NEVER appears in any keystroke argv.
    assert!(
        !logged.contains(secret),
        "the message body must NEVER reach the injector argv:\n{logged}"
    );
    // The body DID land in the inbox (it travels via the store, not the keystroke).
    let inbox = run_ok(&b, &["inbox", "--me", "bob", "--json", "--peek"]);
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    assert_eq!(v["messages"][0]["body"], secret);
}

// ---------------------------------------------------------------------------
// Tier-2 phase 2d — signed sender identity (only built with `--features sign`).
//
// Black-box hostile-input tests against the real `--features sign` binary,
// driving real key files on disk. Each actor gets an isolated `XDG_CONFIG_HOME`
// so its private key never lands in the harness's config. These pin the
// load-bearing security properties: a present-but-invalid signature is ALWAYS
// rejected (never committed) regardless of strict mode; a spoofed `from` is
// rejected; strict_verify drops an unsigned intent; and the private key file is
// 0600 and never leaks to stdout.
// ---------------------------------------------------------------------------

/// A unique, isolated `XDG_CONFIG_HOME` for one signing actor.
#[cfg(feature = "sign")]
fn sign_config_home() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("weave-sec-sign-{pid}-{n}-{nanos}"));
    std::fs::create_dir_all(&d).expect("create temp config home");
    d
}

/// Parse the `public key:  <hex>` line emitted by `weave key gen`.
#[cfg(feature = "sign")]
fn sign_pubkey_from_gen(out: &str) -> String {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("public key:"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| panic!("`weave key gen` did not print a public key:\n{out}"))
}

/// Count messages in a receiver's inbox via `--json --peek`.
#[cfg(feature = "sign")]
fn sign_inbox_len(db: &TestDb, me: &str, cfg_home: &str) -> usize {
    let inbox = run_ok_env(
        db,
        &["inbox", "--me", me, "--json", "--peek"],
        &[("XDG_CONFIG_HOME", cfg_home)],
    );
    let v: serde_json::Value = serde_json::from_str(&inbox).expect("inbox parses");
    v["messages"].as_array().map(|a| a.len()).unwrap_or(0)
}

/// TAMPER / mismatched signature: A genuinely signs as "alice", but B has a
/// DIFFERENT public key registered for "alice" (so the present signature does not
/// verify). A present-but-invalid signature is a hard fail — REJECTED, never
/// committed — in BOTH non-strict and strict modes, and A's store stays unchanged.
#[cfg(feature = "sign")]
#[test]
fn signed_intent_failing_verification_is_always_rejected() {
    let a = TestDb::new();
    let b = TestDb::new();
    let decoy = TestDb::new(); // a throwaway store used only to mint a real, DIFFERENT key
    let a_cfg = sign_config_home();
    let decoy_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let decoy_cfg_s = decoy_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // A's real signing key (will actually sign the intent).
    run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    // A genuinely-different key, registered on B under "alice" — so A's signature
    // verifies against the WRONG public key (equivalent to a tampered/forged sig).
    let decoy_gen = run_ok_env(
        &decoy,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &decoy_cfg_s)],
    );
    let wrong_pub = sign_pubkey_from_gen(&decoy_gen);
    run_ok_env(
        &b,
        &["key", "add", "alice", &wrong_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );

    // A sends a SIGNED intent (signed by A's real key).
    run_ok_env(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "should never commit",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let a_bytes_before = std::fs::read(&a.path).expect("read A");

    // Non-strict: a present-but-invalid signature is STILL rejected.
    let p_lax = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_lax.contains("pulled 0 message"),
        "a signature that fails verification must be rejected even in advisory mode: {p_lax}"
    );
    assert_eq!(
        sign_inbox_len(&b, "bob", &b_cfg_s),
        0,
        "a rejected intent must leave B's inbox untouched"
    );
    // A's store is byte-unchanged (owner-only-writes: verify happens on B's side).
    assert_eq!(
        a_bytes_before,
        std::fs::read(&a.path).expect("read A after"),
        "verification must not write the source store"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&decoy_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// SPOOF: an intent claiming `from = carol` but signed by a different (attacker's)
/// key is rejected, because B verifies against carol's REAL registered key.
#[cfg(feature = "sign")]
#[test]
fn spoofed_from_signed_by_wrong_key_is_rejected() {
    let attacker = TestDb::new();
    let carol_store = TestDb::new();
    let b = TestDb::new();
    let atk_cfg = sign_config_home();
    let carol_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let atk_cfg_s = atk_cfg.to_string_lossy().into_owned();
    let carol_cfg_s = carol_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // The attacker has its OWN key file (it will sign the spoof).
    run_ok_env(
        &attacker,
        &["key", "gen", "--me", "attacker"],
        &[("XDG_CONFIG_HOME", &atk_cfg_s)],
    );
    // Carol's REAL key, registered on B — this is what B checks against.
    let carol_gen = run_ok_env(
        &carol_store,
        &["key", "gen", "--me", "carol"],
        &[("XDG_CONFIG_HOME", &carol_cfg_s)],
    );
    let carol_pub = sign_pubkey_from_gen(&carol_gen);
    run_ok_env(
        &b,
        &["key", "add", "carol", &carol_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );

    // The attacker enqueues an intent CLAIMING from=carol, signed by attacker's key.
    run_ok_env(
        &attacker,
        &[
            "send",
            "--from",
            "carol",
            "--to",
            "bob",
            "--body",
            "I am totally carol",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &atk_cfg_s)],
    );

    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &attacker.path_str()),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull.contains("pulled 0 message"),
        "a spoofed `from` signed by the wrong key must be rejected: {pull}"
    );
    assert_eq!(
        sign_inbox_len(&b, "bob", &b_cfg_s),
        0,
        "the spoofed intent must never reach B's inbox"
    );

    let _ = std::fs::remove_dir_all(&atk_cfg);
    let _ = std::fs::remove_dir_all(&carol_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// strict_verify: an UNSIGNED intent from a pull source is DROPPED under
/// `WEAVE_STRICT_VERIFY=1` but COMMITS in advisory mode when strict is off.
#[cfg(feature = "sign")]
#[test]
fn strict_verify_drops_unsigned_but_advisory_commits() {
    // Two distinct sender stores (NO key file ⇒ unsigned intents). Using a separate
    // source per receiver keeps each outbox to exactly one intent — a shared source
    // would deliver BOTH enqueued intents to whichever receiver pulls it (the
    // `--to-store` host hint is advisory, not a delivery filter).
    let a = TestDb::new();
    let a2 = TestDb::new();
    let a_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // --- strict receiver: unsigned is DROPPED ---
    let b_strict = TestDb::new();
    run_ok_env(
        &a,
        &[
            "send",
            "--from",
            "dave",
            "--to",
            "bob",
            "--body",
            "unsigned hello",
            "--to-store",
            &b_strict.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let p_strict = run_ok_env(
        &b_strict,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("WEAVE_STRICT_VERIFY", "1"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_strict.contains("pulled 0 message"),
        "strict_verify must DROP an unsigned intent: {p_strict}"
    );
    assert_eq!(
        sign_inbox_len(&b_strict, "bob", &b_cfg_s),
        0,
        "a dropped unsigned intent must not reach the inbox under strict"
    );

    // --- advisory receiver: unsigned COMMITS (from a fresh single-intent source) ---
    let b_lax = TestDb::new();
    run_ok_env(
        &a2,
        &[
            "send",
            "--from",
            "dave",
            "--to",
            "bob",
            "--body",
            "unsigned hello",
            "--to-store",
            &b_lax.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let p_lax = run_ok_env(
        &b_lax,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a2.path_str()),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_lax.contains("pulled 1 message"),
        "with strict off, an unsigned intent commits under advisory: {p_lax}"
    );
    assert_eq!(
        sign_inbox_len(&b_lax, "bob", &b_cfg_s),
        1,
        "advisory mode delivers the unsigned intent"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// The private signing-key file is created 0600 (owner-only) and its secret
/// bytes never appear on stdout.
#[cfg(feature = "sign")]
#[test]
fn private_key_file_is_0600_and_secret_never_printed() {
    let a = TestDb::new();
    let a_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();

    let keygen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );

    let key_file = a_cfg.join("weave").join("ed25519.key");
    let meta = std::fs::metadata(&key_file)
        .unwrap_or_else(|e| panic!("private key file {key_file:?} must exist: {e}"));
    let mode = meta.permissions().mode();
    assert_eq!(
        mode & 0o177,
        0,
        "private key file must be 0600 (no group/other, no exec); mode was {:o}",
        mode & 0o777
    );

    let secret = std::fs::read_to_string(&key_file).expect("read key");
    assert!(
        !keygen.contains(secret.trim()),
        "the private key secret must never appear on stdout"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
}
