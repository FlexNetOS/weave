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

use common::{run, run_env, run_in_cwd, run_ok, run_ok_env, McpServer, TestDb};
#[cfg(feature = "surfaces")]
use common::{scrub_env, weave_bin};
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

/// Secret-hygiene (Feature #2): `weave doctor` must NEVER print a configured pull
/// token byte — in `--json`, in the human form, OR in the MCP `weave_doctor` tool
/// result — even with a per-source token, a shared token, AND a per-source timeout
/// all set. The new per-source-timeout observability prints only tier COUNTS + a
/// plain ms range, never a token or a label↔token pairing. Holds on both backends.
#[test]
fn doctor_never_prints_pull_token() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const PER_SOURCE: &str = "doctor-redact-per-source-token-XYZ";
    const SHARED: &str = "doctor-redact-shared-token-QRS";
    let entry = "PROD=libsql://redact.invalid/db";

    let env: &[(&str, &str)] = &[
        ("WEAVE_PEER_DBS", entry),
        ("WEAVE_PULL_TOKEN_PROD", PER_SOURCE),
        ("WEAVE_PULL_TOKEN", SHARED),
        ("WEAVE_PULL_TIMEOUT_MS_PROD", "250"),
        ("WEAVE_PULL_TIMEOUT_MS", "1000"),
    ];

    // (1) doctor --json — neither token in stdout/stderr.
    let (ok_j, out_j, err_j) = run_env(&local, &["doctor", "--json"], env);
    assert!(ok_j, "doctor --json must succeed; stderr:\n{err_j}");
    assert!(
        !out_j.contains(PER_SOURCE),
        "per-source token in json stdout: {out_j}"
    );
    assert!(
        !err_j.contains(PER_SOURCE),
        "per-source token in json stderr: {err_j}"
    );
    assert!(
        !out_j.contains(SHARED),
        "shared token in json stdout: {out_j}"
    );
    assert!(
        !err_j.contains(SHARED),
        "shared token in json stderr: {err_j}"
    );

    // (2) doctor (human) — neither token in stdout/stderr.
    let (ok_h, out_h, err_h) = run_env(&local, &["doctor"], env);
    assert!(ok_h, "human doctor must succeed; stderr:\n{err_h}");
    assert!(
        !out_h.contains(PER_SOURCE),
        "per-source token in human stdout: {out_h}"
    );
    assert!(
        !err_h.contains(PER_SOURCE),
        "per-source token in human stderr: {err_h}"
    );
    assert!(
        !out_h.contains(SHARED),
        "shared token in human stdout: {out_h}"
    );
    assert!(
        !err_h.contains(SHARED),
        "shared token in human stderr: {err_h}"
    );
    // Sanity: the new timeout observability line is actually present (so the
    // redaction assertion above is exercising the new surface, not a no-op).
    assert!(
        out_h.contains("remote timeout:"),
        "human doctor must surface the per-source timeout line: {out_h}"
    );

    // (3) MCP weave_doctor tool result — neither token in the result text.
    let mut mcp = McpServer::spawn_env(&local, env);
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "MCP doctor is not a tool error: {dtext}");
    assert!(
        !dtext.contains(PER_SOURCE),
        "per-source token in MCP doctor result: {dtext}"
    );
    assert!(
        !dtext.contains(SHARED),
        "shared token in MCP doctor result: {dtext}"
    );
    assert!(
        dtext.contains("remote timeout:"),
        "MCP doctor must surface the per-source timeout line: {dtext}"
    );
    mcp.shutdown();
}

/// Secret-hygiene HEADLINE for Feature #9 (the newly-surfaced `pull_from` side):
/// with per-source AND shared tokens set for BOTH `WEAVE_PULL_FROM` and
/// `WEAVE_PEER_DBS`, `weave doctor` must NEVER print a token byte — in `--json`,
/// in the human form, OR in the MCP `weave_doctor` tool result. The new pull-side
/// federation-health block renders only tier COUNTS + a plain ms range, never a
/// token nor a label↔token pairing. The pull block is asserted present so the
/// redaction is exercised against the new surface, not a no-op. Both backends.
#[test]
fn doctor_never_prints_pull_from_token() {
    let local = TestDb::new();
    run_ok(&local, &["register", "--name", "here"]);

    const PULL_PER_SOURCE: &str = "fed9-pull-per-source-token-JJJ";
    const PEER_PER_SOURCE: &str = "fed9-peer-per-source-token-KKK";
    const SHARED: &str = "fed9-shared-token-LLL";

    // BOTH source kinds carry a labelled remote with its OWN per-source token,
    // plus the shared token fallback. `.invalid` hosts; short timeouts.
    let pull_from = "PULLP=libsql://fed9-pull.invalid/db";
    let peer_dbs = "PEERP=libsql://fed9-peer.invalid/db";
    let env: &[(&str, &str)] = &[
        ("WEAVE_PULL_FROM", pull_from),
        ("WEAVE_PULL_TOKEN_PULLP", PULL_PER_SOURCE),
        ("WEAVE_PULL_TIMEOUT_MS_PULLP", "250"),
        ("WEAVE_PEER_DBS", peer_dbs),
        ("WEAVE_PULL_TOKEN_PEERP", PEER_PER_SOURCE),
        ("WEAVE_PULL_TOKEN", SHARED),
        ("WEAVE_PULL_TIMEOUT_MS", "1000"),
    ];

    let assert_token_free = |label: &str, s: &str| {
        assert!(!s.contains(PULL_PER_SOURCE), "pull token in {label}: {s}");
        assert!(!s.contains(PEER_PER_SOURCE), "peer token in {label}: {s}");
        assert!(!s.contains(SHARED), "shared token in {label}: {s}");
    };

    // (1) doctor --json.
    let (ok_j, out_j, err_j) = run_env(&local, &["doctor", "--json"], env);
    assert!(ok_j, "doctor --json must succeed; stderr:\n{err_j}");
    assert_token_free("json stdout", &out_j);
    assert_token_free("json stderr", &err_j);

    // (2) doctor (human) — and confirm the new pull block is actually rendered.
    let (ok_h, out_h, err_h) = run_env(&local, &["doctor"], env);
    assert!(ok_h, "human doctor must succeed; stderr:\n{err_h}");
    assert_token_free("human stdout", &out_h);
    assert_token_free("human stderr", &err_h);
    assert!(
        out_h.contains("pull sources:") && out_h.contains("pull tokens:"),
        "human doctor must render the new pull-side block (so redaction is exercised): {out_h}"
    );

    // (3) MCP weave_doctor tool result.
    let mut mcp = McpServer::spawn_env(&local, env);
    let (derr, dtext) = mcp.call_tool("weave_doctor", serde_json::json!({}));
    assert!(!derr, "MCP doctor is not a tool error: {dtext}");
    assert_token_free("MCP result", &dtext);
    assert!(
        dtext.contains("pull sources:"),
        "MCP doctor must render the new pull-side block: {dtext}"
    );
    mcp.shutdown();
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
// WL-034 — `weave export` HTML bundle: hostile message bodies must be neutralized.
//
// A body that *looks* like a `</script>` breakout or an event-handler payload is
// delivered verbatim into the store (other tests pin that), and then must be
// rendered SAFELY into the offline HTML bundle: the raw breakout sequence
// `</script><script>` must NOT survive un-neutralized anywhere in the file, and a
// raw `<img ... onerror=` tag must NOT survive in the static (noscript) region.
// ---------------------------------------------------------------------------

#[test]
fn export_neutralizes_script_breakout_and_event_handler() {
    let db = TestDb::new();
    // The classic data-block breakout, plus an attribute-handler payload.
    let breakout = "</script><script>document.title='xss'</script>";
    let img = "<img src=x onerror=alert(1)>";

    run_ok(
        &db,
        &[
            "send",
            "--from",
            "attacker",
            "--to",
            "victim",
            &format!("--body={breakout}"),
        ],
    );
    run_ok(
        &db,
        &[
            "send",
            "--from",
            "attacker",
            "--to",
            "victim",
            &format!("--body={img}"),
        ],
    );

    let out_path = std::env::temp_dir().join(format!(
        "weave-sec-export-{}-{}.html",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let out_str = out_path.to_string_lossy().into_owned();

    run_ok(&db, &["export", "--for", "victim", "--out", &out_str]);

    let html = std::fs::read_to_string(&out_path)
        .unwrap_or_else(|e| panic!("export file should exist: {e}"));

    // 1. The attacker's breakout body must NOT survive verbatim. (Note the
    //    document legitimately ends the data block with `</script>` immediately
    //    followed by the client `<script>` — so we assert on the attacker's
    //    *distinctive* breakout string, which carries the injected payload, rather
    //    than the bare `</script><script>` adjacency.)
    assert!(
        !html.contains(breakout),
        "the raw breakout body must not survive verbatim in the export bundle"
    );
    assert!(
        !html.contains("<script>document.title='xss'</script>"),
        "the injected inner <script> must never appear as live markup"
    );
    // The body DID land, in a neutralized form: in the JSON data block `</` is
    // rewritten to `<\/`, and in the noscript region `<`/`>` are html_escape'd.
    assert!(
        html.contains("<\\/script>") || html.contains("&lt;/script&gt;"),
        "the breakout body must appear in a neutralized/escaped form"
    );

    // 2. The raw event-handler tag must NOT survive in the static (noscript) HTML
    //    region — there every field is html_escape'd. (It is legitimately present
    //    RAW inside the `<script type="application/json">` data block, which is NOT
    //    HTML-parsed and is read via textContent; only `</script` could terminate
    //    that block, and `</` is already neutralized above. So we assert on the
    //    noscript region specifically.)
    let noscript = {
        let s = html.find("<noscript>").expect("noscript region present");
        let e = html[s..]
            .find("</noscript>")
            .expect("noscript region closed")
            + s;
        &html[s..e]
    };
    assert!(
        !noscript.contains("<img src=x onerror=alert(1)>"),
        "raw <img ... onerror=...> must be html_escape'd in the static region"
    );
    assert!(
        noscript.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "the img payload must appear html_escape'd in the static region"
    );

    let _ = std::fs::remove_file(&out_path);
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

// ---------------------------------------------------------------------------
// Feature #3 — TIGHTEN signed identity: trust-set strict-by-default, ABSOLUTE
// revocation (R1), FULL-digest trust matching (R3), and secret-free output across
// the new key commands. All `--features sign`, hermetic, through the COMPILED
// binary. These prove the security-load-bearing cells of the verification decision
// table cannot be weakened by configuration.
// ---------------------------------------------------------------------------

/// The FULL-digest fingerprint (`SHA256:<64-hex>`) for `pubkey`, as the binary
/// derives it: `weave key revoke <pubkey>` echoes the normalized full form. The
/// truncated `SHA256:<16-hex>` display form is NEVER trust-matched (R3).
#[cfg(feature = "sign")]
fn sec_full_fp(db: &TestDb, cfg_home: &str, pubkey: &str) -> String {
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

/// FORGED signature under a CONFIGURED TRUST SET is REJECTED (pulled 0, absent from
/// inbox). A trust set must not weaken the present-but-invalid-always-reject rule.
#[cfg(feature = "sign")]
#[test]
fn forged_sig_under_trust_set_is_rejected() {
    let a = TestDb::new(); // alice's real key (signs the intent)
    let decoy = TestDb::new(); // a DIFFERENT key registered on B for alice
    let b = TestDb::new();
    let a_cfg = sign_config_home();
    let decoy_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let decoy_cfg_s = decoy_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
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
    // B trusts the (decoy) fp it actually has registered for alice — so alice IS a
    // trusted sender, yet her real-key signature fails against the wrong pubkey.
    let trusted_full = sec_full_fp(&b, &b_cfg_s, &wrong_pub);

    run_ok_env(
        &a,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "forged-under-trust",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let pull = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &a.path_str()),
            ("WEAVE_TRUST", &trusted_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        pull.contains("pulled 0 message"),
        "a signature that fails verification is rejected even under a trust set: {pull}"
    );
    assert_eq!(
        sign_inbox_len(&b, "bob", &b_cfg_s),
        0,
        "the forged-under-trust intent must never reach B's inbox"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&decoy_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// R1 — ABSOLUTE REVOCATION through the binary: a REVOKED key's VALID-signed message
/// is REJECTED both under strict AND under `WEAVE_STRICT_VERIFY=false`. The disable
/// toggle must NEVER re-admit a revoked key's signed message.
#[cfg(feature = "sign")]
#[test]
fn revoked_key_valid_sig_rejected_even_when_strict_disabled() {
    // `--to-store` deposits the intent into the SENDER's OWN outbox (the store_path is
    // only a host hint), so the pull source is the sender's store. Use TWO distinct
    // sender stores — both signing with alice's SAME key file — so each receiver pulls
    // exactly one intent (a shared source would deliver both).
    let strict_src = TestDb::new();
    let lax_src = TestDb::new();
    let a_cfg = sign_config_home(); // alice's one key file, shared by both sends
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let b_cfg = sign_config_home();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    // Mint alice's key in a dedicated store, then reuse the SAME config home (key
    // file) to sign from both source stores.
    let keygen_store = TestDb::new();
    let agen = run_ok_env(
        &keygen_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let alice_pub = sign_pubkey_from_gen(&agen);

    let b_strict = TestDb::new();
    let b_lax = TestDb::new();
    run_ok_env(
        &b_strict,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &b_lax,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let alice_full = sec_full_fp(&b_strict, &b_cfg_s, &alice_pub);

    // A signs a genuine intent (valid signature) into EACH source store's own outbox.
    for src in [&strict_src, &lax_src] {
        run_ok_env(
            src,
            &[
                "send",
                "--from",
                "alice",
                "--to",
                "bob",
                "--body",
                "revoked-but-validly-signed",
                "--to-store",
                "host-hint",
            ],
            &[("XDG_CONFIG_HOME", &a_cfg_s)],
        );
    }

    // Under STRICT + revoked: rejected.
    let p_strict = run_ok_env(
        &b_strict,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &strict_src.path_str()),
            ("WEAVE_REVOKED", &alice_full),
            ("WEAVE_STRICT_VERIFY", "1"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_strict.contains("pulled 0 message"),
        "a revoked key's valid-signed message is rejected under strict: {p_strict}"
    );
    assert_eq!(
        sign_inbox_len(&b_strict, "bob", &b_cfg_s),
        0,
        "absent under strict"
    );

    // R1 HARD CASE — disabled strict MUST NOT re-admit a revoked key's signed message.
    let p_lax = run_ok_env(
        &b_lax,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &lax_src.path_str()),
            ("WEAVE_REVOKED", &alice_full),
            ("WEAVE_STRICT_VERIFY", "0"),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_lax.contains("pulled 0 message"),
        "R1: a revoked key's valid-signed message is rejected EVEN with WEAVE_STRICT_VERIFY=false: {p_lax}"
    );
    assert_eq!(
        sign_inbox_len(&b_lax, "bob", &b_cfg_s),
        0,
        "R1: the disable toggle must NEVER re-admit a revoked key's signed message"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// R3 — a TRUNCATED or WRONG fingerprint entry does NOT grant trust: trusting only
/// the 16-hex display prefix (not the full digest) leaves a trusted sender's UNSIGNED
/// message COMMITTING via the advisory path (trust never matched), proving truncation
/// can never silently grant trust. The full fp DOES make the unsigned message reject.
#[cfg(feature = "sign")]
#[test]
fn truncated_fingerprint_does_not_grant_trust() {
    let nokey = TestDb::new(); // keyless ⇒ unsigned claim of alice
    let nokey_cfg = sign_config_home();
    let nokey_cfg_s = nokey_cfg.to_string_lossy().into_owned();
    let a = TestDb::new(); // alice's key, registered on B (so a fp exists to (mis)trust)
    let a_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let b_cfg = sign_config_home();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let agen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let alice_pub = sign_pubkey_from_gen(&agen);

    // Derive the FULL fp and a TRUNCATED display form (the 16-hex prefix) of it.
    let b_helper = TestDb::new();
    run_ok_env(
        &b_helper,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    let full = sec_full_fp(&b_helper, &b_cfg_s, &alice_pub); // SHA256:<64-hex>
    let hex = full.strip_prefix("SHA256:").unwrap();
    let truncated = format!("SHA256:{}", &hex[..16]); // the display form — must NOT match

    // Case A: trust ONLY the truncated form ⇒ alice is NOT trusted ⇒ unsigned COMMITS.
    let b_trunc = TestDb::new();
    run_ok_env(
        &b_trunc,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &nokey,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "unsigned-trunc-trust",
            "--to-store",
            &b_trunc.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &nokey_cfg_s)],
    );
    let p_trunc = run_ok_env(
        &b_trunc,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &nokey.path_str()),
            ("WEAVE_TRUST", &truncated),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_trunc.contains("pulled 1 message"),
        "a TRUNCATED fp must NOT grant trust ⇒ the unsigned message stays advisory and commits: {p_trunc}"
    );

    // Case B: trust the FULL form ⇒ alice IS trusted ⇒ unsigned REJECTED (control).
    let nokey2 = TestDb::new();
    let nokey2_cfg = sign_config_home();
    let nokey2_cfg_s = nokey2_cfg.to_string_lossy().into_owned();
    let b_full = TestDb::new();
    run_ok_env(
        &b_full,
        &["key", "add", "alice", &alice_pub],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    run_ok_env(
        &nokey2,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "unsigned-full-trust",
            "--to-store",
            &b_full.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &nokey2_cfg_s)],
    );
    let p_full = run_ok_env(
        &b_full,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &nokey2.path_str()),
            ("WEAVE_TRUST", &full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_full.contains("pulled 0 message"),
        "control: the FULL fp DOES grant trust ⇒ the unsigned message is rejected: {p_full}"
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
    let _ = std::fs::remove_dir_all(&nokey_cfg);
    let _ = std::fs::remove_dir_all(&nokey2_cfg);
}

/// NO private-key bytes EVER appear in the stdout of any key command (gen, show,
/// fingerprint, list, rotate, doctor) — including the rotate `.bak` archive secret.
/// Reads the on-disk secret(s) and asserts they never substring any command's output.
#[cfg(feature = "sign")]
#[test]
fn no_command_ever_prints_a_private_key() {
    let a = TestDb::new();
    let a_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let key_file = a_cfg.join("weave").join("ed25519.key");

    // gen
    let gen = run_ok_env(
        &a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let secret1 = std::fs::read_to_string(&key_file)
        .expect("key")
        .trim()
        .to_string();

    // show, fingerprint, list, doctor — all before rotate, against secret1.
    let show = run_ok_env(
        &a,
        &["key", "show", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let fp = run_ok_env(
        &a,
        &["key", "fingerprint", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let list = run_ok_env(&a, &["key", "list"], &[("XDG_CONFIG_HOME", &a_cfg_s)]);
    let list_json = run_ok_env(
        &a,
        &["key", "list", "--json"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let doctor = run_ok_env(&a, &["doctor"], &[("XDG_CONFIG_HOME", &a_cfg_s)]);
    let doctor_json = run_ok_env(&a, &["doctor", "--json"], &[("XDG_CONFIG_HOME", &a_cfg_s)]);

    // rotate ⇒ writes a NEW secret + archives secret1 to a .bak.
    let rotate = run_ok_env(
        &a,
        &["key", "rotate", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    );
    let secret2 = std::fs::read_to_string(&key_file)
        .expect("new key")
        .trim()
        .to_string();

    // Gather every on-disk secret (current + all .bak archives).
    let mut secrets = vec![secret1.clone(), secret2.clone()];
    for e in std::fs::read_dir(a_cfg.join("weave"))
        .expect("dir")
        .flatten()
    {
        let p = e.path();
        if p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(".bak"))
            .unwrap_or(false)
        {
            if let Ok(s) = std::fs::read_to_string(&p) {
                secrets.push(s.trim().to_string());
            }
        }
    }

    let outputs = [
        ("gen", gen),
        ("show", show),
        ("fingerprint", fp),
        ("list", list),
        ("list --json", list_json),
        ("doctor", doctor),
        ("doctor --json", doctor_json),
        ("rotate", rotate),
    ];
    for s in &secrets {
        assert!(!s.is_empty(), "a key file must hold a secret");
        for (name, out) in &outputs {
            assert!(
                !out.contains(s.as_str()),
                "the private key secret must NEVER appear in `weave key {name}` stdout"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&a_cfg);
}

// ---------------------------------------------------------------------------
// N. Session-tag hardening — a hostile cwd-derived git tag is BOUNDED + control-
//    free + non-fatal + never injected, and never reaches a shell.
// ---------------------------------------------------------------------------

/// `sanitize_tag`'s cap (mirrors `store::MAX_*_LEN`). The stored tag must never
/// exceed this many CHARS regardless of how large the cwd-derived value was.
const TAG_CAP: usize = 128;

/// Build a temp cwd whose `.git` FILE encodes a HOSTILE linked-worktree id: the
/// `<name>` segment carries shell metacharacters, quotes, backticks, a `$(...)`
/// substitution, control characters, and is wildly oversized. The `.git`-file
/// parser splits the worktree id on `/`, so the id stays a single path segment;
/// everything else is hostile-but-in-segment. Returns (dir, the raw hostile name).
fn hostile_worktree_cwd() -> (std::path::PathBuf, String) {
    // No `/` (that would split the worktree id) but every other nasty byte:
    // control chars (handled by sanitize), shell metacharacters, oversized length.
    let hostile = format!(
        "evil;`id`$(rm -rf ~)\"'\t\x07{}",
        "A".repeat(4096) // wildly over the 128-char cap
    );
    let dir = std::env::temp_dir().join(format!(
        "weave-sec-tag-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create hostile cwd");
    std::fs::write(
        dir.join(".git"),
        format!("gitdir: /fixture/main/.git/worktrees/{hostile}/.git\n"),
    )
    .expect("write hostile .git file");
    (dir, hostile)
}

/// A hostile cwd-derived worktree tag is TRUNCATED + control-stripped and stored
/// BOUNDED — never rejected-fatal, and the stored value is control-free and within
/// the cap. Registration with such a tag must succeed (graceful), and the hostile
/// raw value must never appear verbatim in any output.
#[test]
fn hostile_cwd_worktree_tag_is_bounded_and_nonfatal() {
    let db = TestDb::new();
    let (dir, hostile) = hostile_worktree_cwd();

    // Registration captures the hostile tag from cwd. It must SUCCEED (the tag is
    // descriptive + sanitized, never an identity that hard-fails).
    let (ok, _out, err) = run_in_cwd(&db, &["register", "--name", "victim"], &dir);
    assert!(
        ok,
        "register with a hostile cwd tag must be non-fatal: stderr={err}"
    );

    // The stored worktree tag, read back via peers --json, is BOUNDED and control-
    // free — never the raw hostile string.
    let peers = run_ok(&db, &["peers", "--json"]);
    let pv: serde_json::Value = serde_json::from_str(&peers).expect("peers --json parses");
    let row = pv
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "victim")
        .expect("victim peer present");
    let stored = row["worktree"].as_str().expect("worktree tag is a string");
    assert!(
        stored.chars().count() <= TAG_CAP,
        "stored worktree tag must be capped at {TAG_CAP} chars, got {}",
        stored.chars().count()
    );
    assert!(
        !stored.chars().any(|c| c.is_control()),
        "stored worktree tag must be control-free, got {stored:?}"
    );
    assert_ne!(
        stored, hostile,
        "the raw oversized hostile value must never be stored verbatim"
    );

    // The raw hostile string (oversized) must not appear in any surface output.
    let scan = run_ok(&db, &["scan", "--json"]);
    assert!(
        !scan.contains(&hostile),
        "the raw hostile tag must never be re-emitted by scan"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The hostile tag never reaches a shell: registering / scanning with it spawns no
/// extra child process and never errors. We assert the process-level safety by the
/// absence of any shell-side effect — the hostile `$(rm -rf ~)` / backtick `id`
/// substitution must be inert (stored as inert text, never executed). We prove this
/// by confirming a sentinel file the substitution *would* create is absent and the
/// commands complete successfully.
#[test]
fn hostile_cwd_tag_never_reaches_a_shell() {
    let db = TestDb::new();
    // Craft a cwd whose worktree-id segment, if it ever hit `sh -c`, would create a
    // sentinel file in the cwd. Because weave NEVER shells out (argv-only git), the
    // sentinel must NOT appear.
    let dir = std::env::temp_dir().join(format!(
        "weave-sec-noshell-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create cwd");
    let sentinel = dir.join("PWNED");
    // `$(touch PWNED)` would create ./PWNED only if the tag ever reached a shell.
    let payload = format!(
        "gitdir: /fixture/main/.git/worktrees/{}/.git\n",
        "wt$(touch PWNED)`touch PWNED`;touch PWNED"
    );
    std::fs::write(dir.join(".git"), payload).expect("write .git");

    let (ok, _o, err) = run_in_cwd(&db, &["register", "--name", "shellsafe"], &dir);
    assert!(ok, "register must not error on a metacharacter tag: {err}");
    let (ok2, _o2, err2) = run_in_cwd(&db, &["scan"], &dir);
    assert!(ok2, "scan must not error on a metacharacter tag: {err2}");

    assert!(
        !sentinel.exists(),
        "a shell substitution in the tag must NEVER execute (no PWNED sentinel)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Feature #7 — MULTI-KEY REGISTRY security. The registry now holds several
// pubkeys per identity; these prove the additivity NEVER weakens verification:
//   (1) a REVOKED key inside a multi-key set can NEVER grant a commit, even
//       though it cryptographically verifies (R1 absolute revocation), and the
//       other registered key is unaffected;
//   (2) NO private-key bytes ever appear in `key add` / `key list` / `key remove`
//       / `doctor` stdout (read the on-disk secret hex, assert it never substrings
//       any output);
//   (3) a signature that verifies against NONE of the registered keys is
//       rejected (forgery) even when several keys are registered.
// All `--features sign`, hermetic, per-actor isolated XDG_CONFIG_HOME, real key
// files on disk. Drives the COMPILED binary end-to-end.
// ---------------------------------------------------------------------------

/// The FULL-digest fingerprint (`SHA256:<64-hex>`) for `pubkey`, as the binary
/// derives it (`weave key revoke <pubkey>` echoes the canonical `WEAVE_REVOKED=`
/// value). Never a hand-rolled hash — exactly what production matches against (R3).
#[cfg(feature = "sign")]
fn sign_full_fp(db: &TestDb, cfg_home: &str, pubkey: &str) -> String {
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

/// #7 R1 in a MULTI-KEY set: a receiver registers BOTH alice's old and new key;
/// once the OLD fp is `WEAVE_REVOKED`, an intent signed by the OLD key is REJECTED
/// (a revoked key can never grant a commit, even in a multi-key set and even though
/// it verifies), while a NEW-key signed intent still commits. The source store is
/// byte-unchanged (owner-only-writes).
#[cfg(feature = "sign")]
#[test]
fn revoked_key_in_multikey_set_never_commits() {
    let old_store = TestDb::new();
    let new_store = TestDb::new();
    let b = TestDb::new();
    let old_cfg = sign_config_home();
    let new_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let old_cfg_s = old_cfg.to_string_lossy().into_owned();
    let new_cfg_s = new_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let old_pub = sign_pubkey_from_gen(&run_ok_env(
        &old_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    ));
    let new_pub = sign_pubkey_from_gen(&run_ok_env(
        &new_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    ));

    // BOTH keys registered for the SAME identity at the receiver.
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
    let old_full = sign_full_fp(&b, &b_cfg_s, &old_pub);

    // OLD key signs an intent into B's pull source.
    run_ok_env(
        &old_store,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "revoked-must-not-commit",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &old_cfg_s)],
    );
    let old_src_before = std::fs::read(&old_store.path).expect("read old source");

    // Pull with the OLD fp REVOKED: the message must be REJECTED (R1), even though
    // the old key is registered AND the signature cryptographically verifies.
    let p_rev = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &old_store.path_str()),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_rev.contains("pulled 0 message"),
        "a REVOKED key in a multi-key set can NEVER grant a commit (R1): {p_rev}"
    );
    assert_eq!(
        sign_inbox_len(&b, "bob", &b_cfg_s),
        0,
        "the revoked-key message must leave B's inbox empty"
    );
    // Owner-only-writes: verification never mutates the source store.
    assert_eq!(
        old_src_before,
        std::fs::read(&old_store.path).expect("read old source after"),
        "verification must not write the pulled-from source store"
    );

    // The OTHER registered key is unaffected: a NEW-key signed intent still commits
    // under the SAME revocation (old fp revoked, new key registered & not revoked).
    run_ok_env(
        &new_store,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "new-key-ok",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &new_cfg_s)],
    );
    let p_new = run_ok_env(
        &b,
        &["pull", "--me", "bob"],
        &[
            ("WEAVE_PULL_FROM", &new_store.path_str()),
            ("WEAVE_REVOKED", &old_full),
            ("XDG_CONFIG_HOME", &b_cfg_s),
        ],
    );
    assert!(
        p_new.contains("pulled 1 message"),
        "the non-revoked registered key in the set still commits: {p_new}"
    );

    let _ = std::fs::remove_dir_all(&old_cfg);
    let _ = std::fs::remove_dir_all(&new_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// #7 secret hygiene: NO private-key bytes ever appear in `key add`, `key list`
/// (text + `--json`), `key remove`, or `doctor` (text + `--json`) stdout. We read
/// the actual secret hex written to disk by `key gen` and assert it never substrings
/// any of those outputs, across a MULTI-KEY registry (two keys for one identity).
#[cfg(feature = "sign")]
#[test]
fn multikey_key_and_doctor_surfaces_never_leak_the_secret() {
    let gen_a = TestDb::new();
    let gen_b = TestDb::new();
    let b = TestDb::new();
    let a_cfg = sign_config_home();
    let bb_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let a_cfg_s = a_cfg.to_string_lossy().into_owned();
    let bb_cfg_s = bb_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let k1 = sign_pubkey_from_gen(&run_ok_env(
        &gen_a,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &a_cfg_s)],
    ));
    let k2 = sign_pubkey_from_gen(&run_ok_env(
        &gen_b,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &bb_cfg_s)],
    ));

    // The two on-disk SECRET hex strings — must never appear in any surface below.
    let secret1 = std::fs::read_to_string(a_cfg.join("weave").join("ed25519.key"))
        .expect("key file written")
        .trim()
        .to_string();
    let secret2 = std::fs::read_to_string(bb_cfg.join("weave").join("ed25519.key"))
        .expect("key file written")
        .trim()
        .to_string();
    assert!(!secret1.is_empty() && !secret2.is_empty());

    let assert_clean = |label: &str, out: &str| {
        assert!(
            !out.contains(&secret1) && !out.contains(&secret2),
            "{label} leaked a private key:\n{out}"
        );
    };

    // `key add` (twice — multi-key registry) is secret-free.
    let add1 = run_ok_env(
        &b,
        &["key", "add", "alice", &k1],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    assert_clean("key add #1", &add1);
    let add2 = run_ok_env(
        &b,
        &["key", "add", "alice", &k2],
        &[("XDG_CONFIG_HOME", &b_cfg_s)],
    );
    assert_clean("key add #2", &add2);

    // `key list` (text + json) over a multi-key identity is secret-free.
    assert_clean(
        "key list",
        &run_ok_env(&b, &["key", "list"], &[("XDG_CONFIG_HOME", &b_cfg_s)]),
    );
    assert_clean(
        "key list --json",
        &run_ok_env(
            &b,
            &["key", "list", "--json"],
            &[("XDG_CONFIG_HOME", &b_cfg_s)],
        ),
    );

    // `doctor` (text + json) reports multi-key COUNTS only — secret-free.
    assert_clean(
        "doctor",
        &run_ok_env(&b, &["doctor"], &[("XDG_CONFIG_HOME", &b_cfg_s)]),
    );
    assert_clean(
        "doctor --json",
        &run_ok_env(&b, &["doctor", "--json"], &[("XDG_CONFIG_HOME", &b_cfg_s)]),
    );

    // `key remove` is secret-free (and the surviving key's list stays clean).
    assert_clean(
        "key remove",
        &run_ok_env(
            &b,
            &["key", "remove", "alice", &k1],
            &[("XDG_CONFIG_HOME", &b_cfg_s)],
        ),
    );
    assert_clean(
        "key list after remove",
        &run_ok_env(&b, &["key", "list"], &[("XDG_CONFIG_HOME", &b_cfg_s)]),
    );

    let _ = std::fs::remove_dir_all(&a_cfg);
    let _ = std::fs::remove_dir_all(&bb_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

/// #7 forgery in a multi-key set: a signature that verifies against NONE of the
/// (several) registered keys is REJECTED — multiple registered keys do not widen
/// the door to an unregistered signer.
#[cfg(feature = "sign")]
#[test]
fn signed_by_unregistered_key_rejected_even_with_multikey_set() {
    let k1_store = TestDb::new();
    let k2_store = TestDb::new();
    let attacker = TestDb::new();
    let b = TestDb::new();
    let k1_cfg = sign_config_home();
    let k2_cfg = sign_config_home();
    let atk_cfg = sign_config_home();
    let b_cfg = sign_config_home();
    let k1_cfg_s = k1_cfg.to_string_lossy().into_owned();
    let k2_cfg_s = k2_cfg.to_string_lossy().into_owned();
    let atk_cfg_s = atk_cfg.to_string_lossy().into_owned();
    let b_cfg_s = b_cfg.to_string_lossy().into_owned();

    let k1 = sign_pubkey_from_gen(&run_ok_env(
        &k1_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &k1_cfg_s)],
    ));
    let k2 = sign_pubkey_from_gen(&run_ok_env(
        &k2_store,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &k2_cfg_s)],
    ));
    // The attacker has its OWN key, which is NOT registered for alice.
    run_ok_env(
        &attacker,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &atk_cfg_s)],
    );

    // B registers alice's two LEGITIMATE keys (multi-key set).
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

    // The attacker signs as "alice" with its UNREGISTERED key.
    run_ok_env(
        &attacker,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            "forged-claim",
            "--to-store",
            &b.path_str(),
        ],
        &[("XDG_CONFIG_HOME", &atk_cfg_s)],
    );

    // Advisory (non-strict) and strict both REJECT: the sig matches none of the
    // registered keys ⇒ present-but-invalid ⇒ hard fail.
    for strict in ["0", "1"] {
        let p = run_ok_env(
            &b,
            &["pull", "--me", "bob"],
            &[
                ("WEAVE_PULL_FROM", &attacker.path_str()),
                ("WEAVE_STRICT_VERIFY", strict),
                ("XDG_CONFIG_HOME", &b_cfg_s),
            ],
        );
        assert!(
            p.contains("pulled 0 message"),
            "a sig matching NO registered key is rejected (strict={strict}): {p}"
        );
    }
    assert_eq!(
        sign_inbox_len(&b, "bob", &b_cfg_s),
        0,
        "no forged message reaches the inbox"
    );

    let _ = std::fs::remove_dir_all(&k1_cfg);
    let _ = std::fs::remove_dir_all(&k2_cfg);
    let _ = std::fs::remove_dir_all(&atk_cfg);
    let _ = std::fs::remove_dir_all(&b_cfg);
}

// ---------------------------------------------------------------------------
// #11 observed-revocation audit log — security properties: the `audit` / `doctor`
// surfaces are SECRET-FREE (fingerprints + public labels + counts only, never a
// private key) and BOUNDED (a huge `--limit` cannot trigger an unbounded response;
// captured identity/fp/source are clamped at the write seam).
// ---------------------------------------------------------------------------

/// SECRET-FREE: after generating a key and declaring its revocation, neither
/// `weave audit revocations` (human + json) nor `weave doctor` (human + json) ever
/// leaks the on-disk PRIVATE key. Output carries only the `SHA256:` fingerprint,
/// public identities/labels and counts.
#[cfg(feature = "sign")]
#[test]
fn audit_and_doctor_output_is_secret_free() {
    let db = TestDb::new();
    let cfg = sign_config_home();
    let cfg_s = cfg.to_string_lossy().into_owned();

    let keygen = run_ok_env(
        &db,
        &["key", "gen", "--me", "alice"],
        &[("XDG_CONFIG_HOME", &cfg_s)],
    );
    let pubkey = sign_pubkey_from_gen(&keygen);
    let secret = std::fs::read_to_string(cfg.join("weave").join("ed25519.key")).expect("key file");
    let secret = secret.trim().to_string();
    assert!(!secret.is_empty(), "key file holds the secret");

    // Register the key and declare it revoked (records a declared audit event).
    run_ok_env(
        &db,
        &["key", "add", "alice", &pubkey],
        &[("XDG_CONFIG_HOME", &cfg_s)],
    );
    let revoke = run_ok_env(
        &db,
        &["key", "revoke", &pubkey],
        &[("XDG_CONFIG_HOME", &cfg_s)],
    );
    let full_fp = revoke
        .lines()
        .find_map(|l| l.trim().strip_prefix("WEAVE_REVOKED="))
        .map(|s| s.trim().to_string())
        .expect("revoke echoes the full fp");

    let surfaces = [
        run_ok(&db, &["audit", "revocations"]),
        run_ok(&db, &["audit", "revocations", "--json"]),
        run_ok_env(
            &db,
            &["doctor"],
            &[("WEAVE_REVOKED", &full_fp), ("XDG_CONFIG_HOME", &cfg_s)],
        ),
        run_ok_env(
            &db,
            &["doctor", "--json"],
            &[("WEAVE_REVOKED", &full_fp), ("XDG_CONFIG_HOME", &cfg_s)],
        ),
    ];
    for out in &surfaces {
        assert!(
            !out.contains(&secret),
            "the private key must NEVER appear in audit/doctor output:\n{out}"
        );
    }
    // The audit surfaces DO carry the public fingerprint (sanity that we tested a
    // surface that actually contains revocation data).
    assert!(
        surfaces[1].contains("SHA256:"),
        "audit json carries the public fingerprint: {}",
        surfaces[1]
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

/// BOUNDED: a hostile/oversized `--limit` to `weave audit revocations` cannot
/// produce an unbounded response — the CLI clamps to the read cap. With a handful of
/// events present, an absurd limit returns them without error and never more than the
/// cap. (The per-field write-seam clamp is unit-proven in the store; this asserts the
/// CLI read bound end-to-end.)
#[cfg(feature = "sign")]
#[test]
fn audit_revocations_limit_is_bounded_end_to_end() {
    let db = TestDb::new();
    let cfg = sign_config_home();
    let cfg_s = cfg.to_string_lossy().into_owned();

    // Declare a few revokes so the log has rows.
    for seed in 0..3u8 {
        let kdb = TestDb::new();
        let kcfg = sign_config_home();
        let kcfg_s = kcfg.to_string_lossy().into_owned();
        let pk = sign_pubkey_from_gen(&run_ok_env(
            &kdb,
            &["key", "gen", "--me", &format!("u{seed}")],
            &[("XDG_CONFIG_HOME", &kcfg_s)],
        ));
        run_ok_env(&db, &["key", "revoke", &pk], &[("XDG_CONFIG_HOME", &cfg_s)]);
        let _ = std::fs::remove_dir_all(&kcfg);
    }

    // An absurdly large limit must NOT error and must return a bounded result.
    let json = run_ok(
        &db,
        &["audit", "revocations", "--json", "--limit", "100000000"],
    );
    let v: serde_json::Value = serde_json::from_str(&json).expect("audit --json parses");
    let n = v["revocations"].as_array().unwrap().len();
    assert_eq!(
        n, 3,
        "over-cap limit returns only what exists, bounded: {json}"
    );
    assert_eq!(v["count"], 3);

    // A negative limit is also bounded (clamped, no unbounded scan, no panic).
    // (`--limit=-1` form: clap reads a bare `-1` as a flag.)
    let neg = run_ok(&db, &["audit", "revocations", "--json", "--limit=-1"]);
    let nv: serde_json::Value = serde_json::from_str(&neg).expect("audit --json parses");
    assert!(
        nv["revocations"].as_array().unwrap().len() <= 3,
        "negative limit stays bounded: {neg}"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

// ---------------------------------------------------------------------------
// P1 tracked ask/answer/ack — security / hardening
// ---------------------------------------------------------------------------

/// An ask question body that LOOKS like a CLI flag is delivered byte-for-byte to
/// the askee and is never parsed as a flag (no-shell: the body never reaches a
/// command line). Mirrors the `send` verbatim-delivery proof for the ask path.
#[test]
fn ask_body_is_delivered_verbatim() {
    let db = TestDb::new();
    let payload = "--to=victim --body=pwned; rm -rf / `id`";
    let opened = run_ok(
        &db,
        &[
            "ask",
            "--from",
            "attacker",
            "--to",
            "bob",
            &format!("--body={payload}"),
        ],
    );
    assert!(
        opened.contains("attacker -> bob"),
        "ask route confirmed: {opened:?}"
    );
    // The askee receives the question body unchanged (no flag-parsing, no shell).
    let got = only_body(&db, "bob");
    assert_eq!(
        got, payload,
        "an ask body must arrive byte-for-byte, never parsed as a flag or run by a shell"
    );
}

/// An ask body over `MAX_BODY` (65536) is rejected, not stored — the same cap the
/// `send` path enforces, applied through `store.ask`.
#[test]
fn ask_oversized_body_is_rejected() {
    let db = TestDb::new();
    let big = "x".repeat(70_000); // > MAX_BODY (65536)
    let (ok, _out, _err) = run(
        &db,
        &["ask", "--from", "a", "--to", "b", &format!("--body={big}")],
    );
    assert!(!ok, "an oversized ask body must be rejected, not stored");
    // Nothing landed in b's inbox.
    let (peek_ok, inbox, _e) = run(&db, &["inbox", "--me", "b", "--peek"]);
    assert!(peek_ok);
    assert!(
        !inbox.contains("xxxxx"),
        "the oversized ask must not have been persisted: {inbox:?}"
    );
}

/// An oversized askee identity is rejected by the cap (`MAX_IDENT`), never bound.
#[test]
fn ask_oversized_identity_is_rejected() {
    let db = TestDb::new();
    let giant = "x".repeat(100_000);
    let (ok, _out, err) = run(&db, &["ask", "--from", "a", "--to", &giant, "--body", "q"]);
    assert!(!ok, "an oversized askee identity must be rejected");
    assert!(
        !err.contains("panicked"),
        "clean rejection, not a panic: {err:?}"
    );
}

#[test]
fn idempotency_key_oversized_is_rejected() {
    let db = TestDb::new();
    let giant = "x".repeat(100_000);
    let (ok, _out, err) = run(
        &db,
        &[
            "send",
            "--from",
            "a",
            "--to",
            "b",
            "--body",
            "x",
            "--idempotency-key",
            &giant,
        ],
    );
    assert!(!ok, "oversized idempotency key must be rejected");
    assert!(!err.contains("panicked"), "clean rejection: {err:?}");
}

#[test]
fn idempotency_key_hostile_is_rejected() {
    let db = TestDb::new();
    for bad in ["key\nline", ""] {
        let (ok, _out, err) = run(
            &db,
            &[
                "send",
                "--from",
                "a",
                "--to",
                "b",
                "--body",
                "x",
                "--idempotency-key",
                bad,
            ],
        );
        assert!(!ok, "hostile idempotency key {bad:?} must be rejected");
        assert!(!err.contains("panicked"), "clean rejection: {err:?}");
    }
}

/// A hostile correlation id (shell metacharacters / oversized) is rejected by
/// `ask_id_valid` BEFORE any DB bind on every reference path
/// (`answer`/`ack`/`ask-get`). The metachar string never reaches a `Command`
/// (no-shell) nor a SQL bind, and the failure is a clean non-zero exit.
#[test]
fn ask_hostile_correlation_id_is_rejected_before_bind() {
    let db = TestDb::new();
    let hostiles = [
        "ask; rm -rf /",
        "ask`id`",
        "ask$(whoami)",
        "ask|cat /etc/passwd",
        "ask 1 2 3",
        "../../etc/passwd",
        "'; DROP TABLE asks;--",
    ];
    for bad in hostiles {
        let (ok, _o, err) = run(&db, &["ack", "--from", "a", "--id", bad]);
        assert!(!ok, "hostile ack id {bad:?} must be rejected");
        assert!(!err.contains("panicked"), "{bad:?} clean error: {err:?}");
        let (ok, _o, _e) = run(&db, &["answer", "--from", "a", "--id", bad, "--body", "x"]);
        assert!(!ok, "hostile answer id {bad:?} must be rejected");
        let (ok, _o, _e) = run(&db, &["ask-get", "--id", bad]);
        assert!(!ok, "hostile ask-get id {bad:?} must be rejected");
    }
    // An oversized (>64) id is likewise rejected before any bind.
    let oversized = "a".repeat(200);
    let (ok, _o, _e) = run(&db, &["ack", "--from", "a", "--id", &oversized]);
    assert!(!ok, "oversized correlation id must be rejected");
}

/// `weave asks` cannot be coerced into an unbounded scan: an absurd `--limit`
/// returns only what exists (clamped), with no panic.
#[test]
fn asks_list_is_bounded() {
    let db = TestDb::new();
    for i in 0..4 {
        run_ok(
            &db,
            &[
                "ask",
                "--from",
                "a",
                "--to",
                "b",
                "--body",
                &format!("q{i}"),
            ],
        );
    }
    let json = run_ok(
        &db,
        &[
            "asks",
            "--me",
            "a",
            "--role",
            "any",
            "--limit",
            "100000000",
            "--json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&json).expect("asks --json parses");
    let n = v["asks"].as_array().unwrap().len();
    assert_eq!(
        n, 4,
        "over-cap limit returns only what exists, bounded: {json}"
    );
}

/// MCP discipline: an ask through the MCP server keeps stdout a pure JSON-RPC
/// stream even when the question body contains secret-looking text — the body is
/// not echoed to stdout outside the structured result frame, and the result frame
/// itself carries only the verdict sentence, not the raw body. (`call_tool`
/// asserts a single parseable frame with the matching id; a stray stdout write
/// would break that.)
#[test]
fn mcp_ask_stdout_is_pure_jsonrpc_no_body_leak() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);
    let secret = "TOPSECRET-PASSWORD-9f3a";
    let (is_err, text) = mcp.call_tool(
        "weave_ask",
        serde_json::json!({"from": "a", "to": "b", "body": secret}),
    );
    assert!(!is_err, "ask is honest success: {text}");
    // The structured result is the ask confirmation + verdict, NOT the raw body.
    assert!(text.contains("Opened ask"), "result frame: {text:?}");
    assert!(
        !text.contains(secret),
        "the question body must not ride the ask RESULT frame: {text:?}"
    );
    mcp.shutdown();
}

/// P2 N-cap enforced from the COMPILED binary: an `ask-many` whose `--to` list
/// exceeds the fanout cap (64) is rejected as a whole-call error (no parent opened,
/// no child storm), never a panic. A within-cap list with a metachar/control-char
/// peer is best-effort per child (that child fails, the call still succeeds), and
/// the question body of a created child never reaches a shell (argv-only).
#[test]
fn ask_many_cap_enforced_and_per_peer_validated() {
    let db = TestDb::new();
    // Build an over-cap (>64) --to argv. Each peer is a distinct valid id so only the
    // COUNT triggers the reject (not per-peer validation).
    let peers: Vec<String> = (0..70).map(|i| format!("peer{i}")).collect();
    let mut args: Vec<&str> = vec!["ask-many", "--from", "a", "--body", "q"];
    for p in &peers {
        args.push("--to");
        args.push(p);
    }
    let (ok, _o, _e) = run(&db, &args);
    assert!(!ok, "an over-cap ask-many fanout must be rejected");

    // Within cap, a control-char peer is a per-child failure, not a whole-call error.
    let (ok, out, _e) = run(
        &db,
        &[
            "ask-many",
            "--from",
            "a",
            "--to",
            "good",
            "--to",
            "bad\nid",
            "--body",
            "q;rm -rf /",
        ],
    );
    assert!(
        ok,
        "best-effort per child: a bad peer does not fail the call"
    );
    assert!(
        out.contains("1 created, 1 failed"),
        "per-child verdict: {out:?}"
    );
    // The metachar body never reached a shell: nothing was deleted, the good child
    // exists and its question is in the inbox verbatim.
    assert!(run_ok(&db, &["inbox", "--me", "good"]).contains("q;rm -rf /"));
}

/// P2 result is BOUNDED + secret-free from the binary: the aggregated `ask-many-result`
/// emits at most `target_count` (≤ cap) child rows and leaks no DB path / key
/// material, even when the question body contains secret-looking text.
#[test]
fn ask_many_result_is_bounded_and_secret_free() {
    let db = TestDb::new();
    let secret = "TOPSECRET-PW-7733";
    let opened = run_ok(
        &db,
        &[
            "ask-many", "--from", "a", "--to", "b", "--to", "c", "--body", secret,
        ],
    );
    let parent = opened
        .split_whitespace()
        .map(|w| w.trim_end_matches([':', '.', ',']))
        .find(|w| w.starts_with("askm_"))
        .expect("parent id")
        .to_string();
    let res = run_ok(&db, &["ask-many-result", "--parent-id", &parent]);
    // Bounded: exactly two child rows (≤ target_count ≤ cap).
    let child_rows = res
        .lines()
        .filter(|l| l.trim_start().starts_with('b') || l.trim_start().starts_with('c'))
        .count();
    assert!(
        child_rows <= 2,
        "child rows bounded by target_count: {res:?}"
    );
    // Secret-free: the human result surfaces state/cids, not the question body or db path.
    assert!(
        !res.contains(secret),
        "question body must not ride the result: {res:?}"
    );
    assert!(!res.contains(".db"), "no db path leak: {res:?}");
}

// ---------------------------------------------------------------------------
// P3 job board — caps, id validation, JSON size cap, bounded list, secret-free.
// ---------------------------------------------------------------------------

#[test]
fn job_caps_and_id_validators_reject_hostile_input() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    // Oversized title rejected (MAX_JOB_TEXT-class cap, never persisted).
    let huge = "x".repeat(70_000);
    let (is_err, _t) = mcp.call_tool(
        "weave_job_create",
        serde_json::json!({"creator": "alice", "title": huge}),
    );
    assert!(is_err, "oversized title must be isError");

    // A valid job to exercise the JSON cap + id validators.
    let (_e, text) = mcp.call_tool(
        "weave_job_create",
        serde_json::json!({"creator": "alice", "title": "ok"}),
    );
    let id = text
        .split_whitespace()
        .find(|w| w.starts_with("job_"))
        .expect("job id")
        .to_string();

    // Oversized result JSON rejected (MAX_JOB_JSON byte cap) — unclaimed job, no token needed.
    let big_json = format!("\"{}\"", "y".repeat(70_000));
    let (is_err, _t) = mcp.call_tool(
        "weave_job_update",
        serde_json::json!({"job_id": id, "result": big_json}),
    );
    assert!(is_err, "oversized result JSON must be isError");

    // A metachar-bearing job id never reaches a bind (validator rejects first).
    for bad in ["job;rm", "job id", "ask_1_2", "../etc"] {
        let (is_err, _t) = mcp.call_tool("weave_job_show", serde_json::json!({"job_id": bad}));
        assert!(is_err, "invalid job id {bad:?} must be isError");
    }

    mcp.shutdown();
}

#[test]
fn job_list_limit_is_bounded_and_output_secret_free() {
    let db = TestDb::new();
    // Create a couple of jobs with a recognizable but non-secret title.
    run_ok(
        &db,
        &["job", "create", "--title", "alpha", "--from", "alice"],
    );
    run_ok(
        &db,
        &["job", "create", "--title", "beta", "--from", "alice"],
    );
    // A huge/negative limit is clamped in the store (no panic, no unbounded scan).
    let out = run_ok(&db, &["job", "list", "--limit", "9999999999"]);
    assert!(out.contains("alpha") && out.contains("beta"));
    // No db path / filesystem leak in the human listing.
    assert!(!out.contains(".db"), "no db path leak: {out:?}");
    let (ok, _o, _e) = run(&db, &["job", "list", "--limit=-1"]);
    assert!(ok, "negative limit clamps rather than erroring");
}

// ---------------------------------------------------------------------------
// P4: circle/role input hardening
// ---------------------------------------------------------------------------

/// A metachar/oversized/control WEAVE_CIRCLE is sanitized to the default circle
/// (never stored raw, never crashes). The peer still registers; its circle reads
/// "default".
#[test]
fn invalid_weave_circle_is_sanitized_to_default() {
    for bad in ["a/b; rm -rf", "a$b", "a\nb", &"x".repeat(200)] {
        let db = TestDb::new();
        // register must not crash on a hostile circle.
        let (ok, _o, _e) = run_env(
            &db,
            &["register"],
            &[("WEAVE_SESSION", "victim"), ("WEAVE_CIRCLE", bad)],
        );
        assert!(ok, "register must not crash on hostile circle {bad:?}");
        // The peer is visible in the default circle (sanitized), JSON confirms.
        let out = run_ok_env(
            &db,
            &["peers", "--json", "--all-circles"],
            &[("WEAVE_SESSION", "victim")],
        );
        let arr: serde_json::Value = serde_json::from_str(&out).expect("peers json");
        let victim = arr
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("victim"))
            .expect("victim present");
        assert_eq!(
            victim.get("circle").and_then(|v| v.as_str()),
            Some("default"),
            "hostile circle {bad:?} must sanitize to default, got {victim}"
        );
    }
}

/// Role is an enum, never free text: there is no CLI/MCP surface to set an
/// arbitrary role. A fresh registration is always role='peer'; only `orchestrator
/// claim` promotes — and only to the single 'orchestrator' label.
#[test]
fn role_is_never_free_text() {
    let db = TestDb::new();
    run_ok_env(&db, &["register"], &[("WEAVE_SESSION", "p1")]);
    let out = run_ok_env(&db, &["peers", "--json"], &[("WEAVE_SESSION", "p1")]);
    let arr: serde_json::Value = serde_json::from_str(&out).unwrap();
    let p1 = arr
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("p1"))
        .unwrap();
    assert_eq!(
        p1.get("role").and_then(|v| v.as_str()),
        Some("peer"),
        "a fresh registration is always 'peer': {p1}"
    );

    // After a claim the role is exactly 'orchestrator' (never an attacker string).
    run_ok_env(&db, &["orchestrator", "claim"], &[("WEAVE_SESSION", "p1")]);
    let out2 = run_ok_env(&db, &["peers", "--json"], &[("WEAVE_SESSION", "p1")]);
    let arr2: serde_json::Value = serde_json::from_str(&out2).unwrap();
    let p1b = arr2
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("p1"))
        .unwrap();
    assert_eq!(
        p1b.get("role").and_then(|v| v.as_str()),
        Some("orchestrator"),
        "claim sets exactly 'orchestrator': {p1b}"
    );
}

/// Orchestrator claim/status output is secret-free (no token/path leakage).
#[test]
fn orchestrator_output_is_secret_free() {
    let db = TestDb::new();
    run_ok_env(
        &db,
        &["register"],
        &[("WEAVE_SESSION", "lead"), ("WEAVE_CIRCLE", "sec")],
    );
    let claimed = run_ok_env(
        &db,
        &["orchestrator", "claim"],
        &[("WEAVE_SESSION", "lead"), ("WEAVE_CIRCLE", "sec")],
    );
    let status = run_ok_env(
        &db,
        &["orchestrator", "status", "--circle", "sec"],
        &[("WEAVE_SESSION", "lead")],
    );
    for out in [&claimed, &status] {
        let low = out.to_lowercase();
        assert!(!low.contains("token"), "no token leakage: {out}");
        assert!(!low.contains("auth"), "no auth leakage: {out}");
        assert!(!out.contains("/home/"), "no filesystem path leakage: {out}");
    }
}

// ─────────────────────────── P5: rich presence security ─────────────────────────

/// Read a peer's description from `peers --json` (the structured surface that carries
/// the read-time-TTL'd view).
fn peer_description(db: &TestDb, name: &str) -> Option<String> {
    let out = run_ok(db, &["peers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("peers --json must parse: {e}\n{out}"));
    v.as_array()?
        .iter()
        .find(|p| p["name"] == name)
        .map(|p| p["description"].as_str().unwrap_or_default().to_string())
}

/// A hostile, oversized description carrying embedded control characters is CAPPED to
/// MAX_DESC_LEN and CONTROL-STRIPPED at the store seam — never errors (truncates), and
/// the stored/surfaced value is bounded + control-free. The `sanitize_tag` idiom.
#[test]
fn hostile_oversized_description_is_capped_and_control_stripped() {
    const MAX_DESC_LEN: usize = 200;
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "dd"]);
    // Oversized + embedded control chars (newline, tab, CR, ESC, bell) + a long run.
    // (NUL is excluded: the OS forbids NUL bytes in a process argument, so it can
    // never reach the sanitizer as an argv value in the first place.)
    let hostile = format!("evil\n\t\r\u{1b}\u{7}{}", "Z".repeat(5_000));
    let (ok, _o, err) = common::run(&db, &["describe", &hostile, "--me", "dd"]);
    assert!(
        ok,
        "describe must succeed with a sanitized description: {err}"
    );

    let desc = peer_description(&db, "dd").expect("dd peer present");
    assert!(
        desc.chars().count() <= MAX_DESC_LEN,
        "stored description must be <= MAX_DESC_LEN chars, got {}: {desc:?}",
        desc.chars().count()
    );
    assert!(
        !desc.chars().any(|c| c.is_control()),
        "stored description must be control-char free: {desc:?}"
    );
}

/// `weave status` rejects a non-enum turn_state at the seam (a hard error, never a
/// panic, never a raw store) — turn_state is an ENUM, not free text.
#[test]
fn status_rejects_non_enum_turn_state_security() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "ss"]);
    // (NUL is excluded: the OS forbids NUL bytes in a process argument, so it can
    // never reach the validator as an argv value in the first place.)
    for bad in ["working;DROP", "../idle", "RUNNING", "idle\u{1b}", "  "] {
        let (ok, _o, _e) = common::run(&db, &["status", bad, "--me", "ss"]);
        assert!(!ok, "a non-enum turn_state {bad:?} must be rejected");
    }
    // No marker ever leaked from a rejected value.
    let peers = run_ok(&db, &["peers"]);
    assert!(
        !peers.contains("[working]") && !peers.contains("[pending]"),
        "a rejected turn_state never surfaces: {peers}"
    );
}

/// OWNER-ONLY: a peer cannot set ANOTHER peer's description/turn_state. The CLI binds
/// the row key to the caller's OWN resolved identity (`--me`), so a describe/status
/// issued as "attacker" never mutates "victim"'s row.
#[test]
fn presence_setters_are_owner_only() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "victim"]);
    run_ok(&db, &["register", "--name", "attacker"]);
    // The attacker sets ITS OWN presence; victim is untouched.
    run_ok(&db, &["describe", "attacker note", "--me", "attacker"]);
    run_ok(&db, &["status", "working", "--me", "attacker"]);
    assert_eq!(
        peer_description(&db, "victim").as_deref(),
        Some(""),
        "victim's description must stay empty (owner-only)"
    );
    // victim's turn_state stays unset (no [working] marker on victim's line).
    let peers = run_ok(&db, &["peers"]);
    // attacker carries the marker; victim's line must NOT.
    let victim_line = peers.lines().find(|l| l.contains("victim")).unwrap_or("");
    assert!(
        !victim_line.contains("[working]"),
        "victim must not inherit the attacker's turn_state: {victim_line}"
    );
}

/// The presence surfaces are SECRET-FREE: a description set from typical text never
/// leaks tokens/auth/filesystem paths into peers/whoami/scan output.
#[test]
fn presence_surfaces_are_secret_free() {
    let db = TestDb::new();
    run_ok(&db, &["register", "--name", "sf"]);
    run_ok(
        &db,
        &["describe", "refactoring the store layer", "--me", "sf"],
    );
    run_ok(&db, &["status", "working", "--me", "sf"]);
    for cmd in [vec!["peers"], vec!["scan"]] {
        let out = run_ok(&db, &cmd);
        let low = out.to_lowercase();
        assert!(!low.contains("token"), "no token leakage in {cmd:?}: {out}");
        assert!(
            !low.contains("secret"),
            "no secret leakage in {cmd:?}: {out}"
        );
        assert!(!out.contains("/home/"), "no path leakage in {cmd:?}: {out}");
    }
}

// ---------------------------------------------------------------------------
// P6 — delivery trace: secret-free, capped, bounded, best-effort.
// ---------------------------------------------------------------------------

/// Extract the leading `#<n>` message id from a notify/send result line.
fn mid_of(text: &str) -> String {
    text.split_whitespace()
        .find_map(|w| {
            let w = w.trim_start_matches('(').trim_start_matches('#');
            let w = w.trim_end_matches([',', '.', ')', ':']);
            w.parse::<i64>().ok().map(|n| n.to_string())
        })
        .unwrap_or_else(|| panic!("no message id in: {text:?}"))
}

/// SECRET-FREE: a hostile/marker body sent via `weave notify` must NEVER appear in
/// any `weave delivery` row (human OR --json) — the trace is metadata-only
/// (ref_id, ref_kind, to_peer, stage, outcome, ts). The body still lives in the
/// inbox (access-controlled), but the transport trace leaks nothing.
#[test]
fn delivery_trace_never_contains_the_body() {
    let db = TestDb::new();
    let marker = "TOPSECRET-DELIVERY-MARKER-7f3a";
    let out = run_ok(
        &db,
        &["notify", "--from", "a", "--to", "b", "--body", marker],
    );
    let mid = mid_of(&out);

    // Human trace.
    let human = run_ok(&db, &["delivery", "--id", &mid]);
    assert!(
        !human.contains(marker),
        "delivery (human) leaked the body: {human}"
    );
    // JSON trace.
    let json = run_ok(&db, &["delivery", "--id", &mid, "--json"]);
    assert!(
        !json.contains(marker),
        "delivery (--json) leaked the body: {json}"
    );
    // Sanity: the body DID persist to the inbox (so we know it was really sent).
    let inbox = run_ok(&db, &["inbox", "--me", "b", "--peek"]);
    assert!(inbox.contains(marker), "body persisted to inbox: {inbox}");
}

/// CAPS + NO-SHELL: an oversized notify body is rejected (never a panic / partial
/// persist). A metachar/space-bearing `to` is NOT shelled (weave never reaches a
/// shell) — it is treated as a literal recipient name, bound as a SQL param, so the
/// notify completes safely without spawning anything. A control-bearing `to` is
/// rejected by `check_ident`.
#[test]
fn notify_caps_and_hostile_target_are_safe() {
    let db = TestDb::new();
    // Oversized body (> MAX_BODY 65536) is rejected.
    let big = "x".repeat(70_000);
    let (ok, _o, err) = run(&db, &["notify", "--from", "a", "--to", "b", "--body", &big]);
    assert!(!ok, "oversized notify body must be rejected");
    assert!(err.contains("too long"), "clear cap error: {err}");

    // A shell-metachar `to` must NEVER be shelled — it is a literal recipient name.
    // The notify succeeds (degrade-to-store) and the message is addressed verbatim,
    // proving the value was bound, not interpreted. The fs sentinel proves nothing
    // ran a shell.
    let sentinel = std::env::temp_dir().join(format!("weave-notify-pwn-{}", std::process::id()));
    let _ = std::fs::remove_file(&sentinel);
    let hostile = format!("evil; touch {}", sentinel.display());
    let (ok, _o, _e) = run(
        &db,
        &["notify", "--from", "a", "--to", &hostile, "--body", "x"],
    );
    assert!(ok, "a metachar target is a literal name, handled safely");
    assert!(
        !sentinel.exists(),
        "no shell ran: the sentinel file must not exist"
    );

    // A control-character target IS rejected by check_ident (never persisted).
    let ctrl = "evil\n\t\u{1b}";
    let (ok, _o, _e) = run(&db, &["notify", "--from", "a", "--to", ctrl, "--body", "x"]);
    assert!(!ok, "control-bearing target must be rejected");
}

/// SECRET-FREE (MCP surface): a hostile body sent through `weave_notify` and then
/// read back through `weave_delivery` never leaks the body byte. Mirrors the CLI
/// secret-free test on the JSON-RPC seam.
#[test]
fn mcp_delivery_trace_is_secret_free() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);
    let marker = "MCP-DELIVERY-SECRET-9b2c";
    let (is_err, text) = mcp.call_tool(
        "weave_notify",
        serde_json::json!({"from": "a", "to": "b", "body": marker}),
    );
    assert!(!is_err, "notify success: {text}");
    let mid: i64 = mid_of(&text).parse().unwrap();
    let (_e, dtext) = mcp.call_tool("weave_delivery", serde_json::json!({"message_id": mid}));
    assert!(
        !dtext.contains(marker),
        "MCP delivery trace leaked the body: {dtext}"
    );
    mcp.shutdown();
}

// ---------------------------------------------------------------------------
// WL-016 scheduler security / hardening
// ---------------------------------------------------------------------------

/// An oversized cron expression ( > MAX_CRON_EXPR_LEN = 64 ) must be rejected
/// by the MCP layer with an isError result rather than being persisted.
#[test]
fn mcp_schedule_oversized_cron_is_rejected() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let huge_cron = "x".repeat(100);
    let (is_err, text) = mcp.call_tool(
        "weave_schedule",
        serde_json::json!({
            "from": "alice",
            "to": "bob",
            "body": "hi",
            "every": huge_cron
        }),
    );
    assert!(
        is_err,
        "weave_schedule with oversized cron must be rejected (isError), got ok: {}",
        &text[..text.len().min(200)]
    );

    mcp.shutdown();
}

/// Schedule with both 'at' and 'every' is rejected (xor requirement).
#[test]
fn mcp_schedule_both_at_and_every_is_rejected() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let (is_err, text) = mcp.call_tool(
        "weave_schedule",
        serde_json::json!({
            "from": "alice",
            "to": "bob",
            "body": "hi",
            "at": 1234567890,
            "every": "@daily"
        }),
    );
    assert!(
        is_err,
        "weave_schedule with both at and every must be rejected, got ok: {text}"
    );
    assert!(
        text.to_lowercase().contains("not both"),
        "rejection should mention 'not both': {text}"
    );

    mcp.shutdown();
}

/// Schedule with neither 'at' nor 'every' is rejected.
#[test]
fn mcp_schedule_neither_at_nor_every_is_rejected() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let (is_err, text) = mcp.call_tool(
        "weave_schedule",
        serde_json::json!({
            "from": "alice",
            "to": "bob",
            "body": "hi"
        }),
    );
    assert!(
        is_err,
        "weave_schedule with neither at nor every must be rejected, got ok: {text}"
    );

    mcp.shutdown();
}

/// An invalid cron expression is rejected by the MCP layer.
#[test]
fn mcp_schedule_invalid_cron_is_rejected() {
    let db = TestDb::new();
    let mut mcp = McpServer::spawn(&db);

    let (is_err, text) = mcp.call_tool(
        "weave_schedule",
        serde_json::json!({
            "from": "alice",
            "to": "bob",
            "body": "hi",
            "every": "not-a-cron"
        }),
    );
    assert!(
        is_err,
        "weave_schedule with invalid cron must be rejected, got ok: {text}"
    );
    assert!(
        text.to_lowercase().contains("not a valid cron"),
        "rejection should mention invalid cron: {text}"
    );

    mcp.shutdown();
}

/// CLI: an oversized body scheduled via the CLI must be rejected (body cap).
#[test]
fn cli_schedule_oversized_body_is_rejected() {
    let db = TestDb::new();
    let huge_body = "x".repeat(70_000);
    let (ok, out, err) = run(
        &db,
        &[
            "schedule",
            "--from",
            "alice",
            "--to",
            "bob",
            "--body",
            &huge_body,
            "--at",
            "1234567890",
        ],
    );
    assert!(
        !ok,
        "schedule with oversized body must fail (non-zero exit): {out}\n{err}"
    );
    assert!(
        (out.to_lowercase().contains("too long") || err.to_lowercase().contains("too long")),
        "rejection should mention length cap: stdout={out}\nstderr={err}"
    );
}

/// CLI: schedule --at with a non-positive timestamp is rejected.
#[test]
fn cli_schedule_non_positive_at_is_rejected() {
    let db = TestDb::new();
    let (ok, out, err) = run(
        &db,
        &[
            "schedule", "--from", "alice", "--to", "bob", "--body", "hi", "--at", "0",
        ],
    );
    assert!(!ok, "schedule with at=0 must fail: {out}\n{err}");
    assert!(
        (out.to_lowercase().contains("positive") || err.to_lowercase().contains("positive")),
        "rejection should mention positive: stdout={out}\nstderr={err}"
    );
}

// ---------------------------------------------------------------------------
// Memory security (WL-017)
// ---------------------------------------------------------------------------

/// CLI: memory write with a path-traversal key is rejected.
#[test]
fn cli_memory_path_traversal_key_rejected() {
    let db = TestDb::new();
    let cfg = std::env::temp_dir().join(format!("weave-mem-sec-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg_s = cfg.to_str().unwrap();

    for bad_key in ["../etc", "foo/bar", "foo\\bar", "..", ""] {
        let (ok, out, err) = run_env(
            &db,
            &[
                "memory", "write", "--scope", "global", "--key", bad_key, "--title", "T", "--body",
                "B",
            ],
            &[("XDG_CONFIG_HOME", cfg_s)],
        );
        assert!(
            !ok,
            "memory write with key '{bad_key}' must fail: {out}\n{err}"
        );
        assert!(
            (out.to_lowercase().contains("traversal")
                || err.to_lowercase().contains("traversal")
                || out.to_lowercase().contains("key")
                || err.to_lowercase().contains("key")),
            "rejection should mention key/traversal: stdout={out}\nstderr={err}"
        );
    }
    std::fs::remove_dir_all(&cfg).ok();
}

/// CLI: memory write with an oversized key is rejected.
#[test]
fn cli_memory_oversized_key_rejected() {
    let db = TestDb::new();
    let cfg = std::env::temp_dir().join(format!("weave-mem-sec-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg_s = cfg.to_str().unwrap();
    let huge_key = "x".repeat(200);

    let (ok, out, err) = run_env(
        &db,
        &[
            "memory", "write", "--scope", "global", "--key", &huge_key, "--title", "T", "--body",
            "B",
        ],
        &[("XDG_CONFIG_HOME", cfg_s)],
    );
    assert!(
        !ok,
        "memory write with oversized key must fail: {out}\n{err}"
    );
    assert!(
        (out.to_lowercase().contains("key") || err.to_lowercase().contains("key")),
        "rejection should mention key: stdout={out}\nstderr={err}"
    );
    std::fs::remove_dir_all(&cfg).ok();
}

/// CLI: memory write with an oversized body is rejected.
#[test]
fn cli_memory_oversized_body_rejected() {
    let db = TestDb::new();
    let cfg = std::env::temp_dir().join(format!("weave-mem-sec-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg_s = cfg.to_str().unwrap();
    let huge_body = "x".repeat(70_000);

    let (ok, out, err) = run_env(
        &db,
        &[
            "memory", "write", "--scope", "global", "--key", "k", "--title", "T", "--body",
            &huge_body,
        ],
        &[("XDG_CONFIG_HOME", cfg_s)],
    );
    assert!(
        !ok,
        "memory write with oversized body must fail: {out}\n{err}"
    );
    assert!(
        (out.to_lowercase().contains("body") || err.to_lowercase().contains("body")),
        "rejection should mention body: stdout={out}\nstderr={err}"
    );
    std::fs::remove_dir_all(&cfg).ok();
}

/// MCP: memory write with a path-traversal key returns isError.
#[test]
fn mcp_memory_path_traversal_key_is_error() {
    let db = TestDb::new();
    let cfg = std::env::temp_dir().join(format!("weave-mem-mcp-sec-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg_s = cfg.to_str().unwrap();
    let mut mcp = McpServer::spawn_env(&db, &[("XDG_CONFIG_HOME", cfg_s)]);

    for bad_key in ["../etc", "foo/bar", "foo\\bar", ".."] {
        let (err, text) = mcp.call_tool(
            "weave_memory_write",
            serde_json::json!({
                "me": "alice",
                "scope": "global",
                "key": bad_key,
                "title": "T",
                "body": "B",
            }),
        );
        assert!(
            err,
            "mcp memory write with key '{bad_key}' must be isError: {text}"
        );
        assert!(
            text.to_lowercase().contains("traversal") || text.to_lowercase().contains("key"),
            "mcp rejection should mention traversal/key: {text}"
        );
    }

    mcp.shutdown();
    std::fs::remove_dir_all(&cfg).ok();
}

/// MCP: memory write with an oversized body returns isError.
#[test]
fn mcp_memory_oversized_body_is_error() {
    let db = TestDb::new();
    let cfg = std::env::temp_dir().join(format!("weave-mem-mcp-sec-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg_s = cfg.to_str().unwrap();
    let mut mcp = McpServer::spawn_env(&db, &[("XDG_CONFIG_HOME", cfg_s)]);
    let huge_body = "x".repeat(70_000);

    let (err, text) = mcp.call_tool(
        "weave_memory_write",
        serde_json::json!({
            "me": "alice",
            "scope": "global",
            "key": "k",
            "title": "T",
            "body": huge_body,
        }),
    );
    assert!(
        err,
        "mcp memory write with oversized body must be isError: {text}"
    );
    assert!(
        text.to_lowercase().contains("body"),
        "mcp rejection should mention body: {text}"
    );

    mcp.shutdown();
    std::fs::remove_dir_all(&cfg).ok();
}

// ---------------------------------------------------------------------------
// WL-047: spawn allowlist + trusted-program + argv-cap + id_valid kill guard
// ---------------------------------------------------------------------------

/// A fake `tmux` that echoes `%9` on a spawn verb so a *permitted* spawn can be
/// observed to fire, and logs argv. Reused across the WL-047 security cases.
fn make_fake_tmux_spawning(log_path: &std::path::Path) -> std::path::PathBuf {
    let dir = TestDb::new().path_str();
    let dir = std::path::PathBuf::from(format!("{dir}.muxbin"));
    std::fs::create_dir_all(&dir).expect("create fake-mux dir");
    let script = dir.join("tmux");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *split-window*|*new-window*) echo '%9' ;;\nesac\nexit 0\n",
        log_path.display()
    );
    std::fs::write(&script, body).expect("write fake tmux");
    let mut perms = std::fs::metadata(&script).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod +x fake tmux");
    dir
}

/// A spawn whose cwd ESCAPES `spawn_allowed_dirs` via `..` traversal must be denied
/// (canonicalization resolves `..`, so the prefix check fails) and launch nothing.
#[test]
fn spawn_cwd_dotdot_traversal_is_denied() {
    let db = TestDb::new();
    let log = std::path::PathBuf::from(format!("{}.tmuxlog", TestDb::new().path_str()));
    let _ = std::fs::remove_file(&log);
    let fake = make_fake_tmux_spawning(&log);

    let base = std::path::PathBuf::from(format!("{}.base", TestDb::new().path_str()));
    let allow = base.join("allow");
    let escape = base.join("escape");
    std::fs::create_dir_all(&allow).unwrap();
    std::fs::create_dir_all(&escape).unwrap();
    let traversal = allow.join("..").join("escape");

    let (ok, _out, err) = run_with_fake_mux(
        &db,
        &fake,
        &[
            ("TMUX_PANE", "%1"),
            ("WEAVE_SPAWN_DIRS", allow.to_str().unwrap()),
        ],
        &[
            "spawn",
            "--name",
            "esc",
            "--mux",
            "tmux",
            "--cwd",
            traversal.to_str().unwrap(),
            "--cmd",
            "echo",
            "hi",
        ],
    );
    assert!(!ok, "a `..`-traversal cwd must be denied");
    assert!(
        err.contains("refusing to spawn") || err.contains("spawn_allowed_dirs"),
        "denial should reference the allowlist: {err:?}"
    );
    let logged = read_mux_log(&log);
    assert!(
        !logged.contains("split-window"),
        "no launch on a denied cwd:\n{logged}"
    );
}

/// A spawn whose cwd is a SYMLINK pointing outside `spawn_allowed_dirs` is denied
/// (canonicalize follows the symlink to its real, disallowed target).
#[test]
fn spawn_cwd_symlink_escape_is_denied() {
    let db = TestDb::new();
    let log = std::path::PathBuf::from(format!("{}.tmuxlog", TestDb::new().path_str()));
    let _ = std::fs::remove_file(&log);
    let fake = make_fake_tmux_spawning(&log);

    let allow = std::path::PathBuf::from(format!("{}.allow", TestDb::new().path_str()));
    let outside = std::path::PathBuf::from(format!("{}.outside", TestDb::new().path_str()));
    std::fs::create_dir_all(&allow).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let link = allow.join("sneaky");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let (ok, _out, err) = run_with_fake_mux(
        &db,
        &fake,
        &[
            ("TMUX_PANE", "%1"),
            ("WEAVE_SPAWN_DIRS", allow.to_str().unwrap()),
        ],
        &[
            "spawn",
            "--name",
            "sym",
            "--mux",
            "tmux",
            "--cwd",
            link.to_str().unwrap(),
            "--cmd",
            "echo",
            "hi",
        ],
    );
    assert!(!ok, "a symlink-escape cwd must be denied");
    assert!(
        err.contains("refusing to spawn") || err.contains("spawn_allowed_dirs"),
        "denial should reference the allowlist: {err:?}"
    );
    let logged = read_mux_log(&log);
    assert!(
        !logged.contains("split-window"),
        "no launch on a symlink-escape cwd:\n{logged}"
    );
}

/// A child program (argv[0]) that does NOT live in a trusted directory is rejected —
/// a spawn must never launch an arbitrary binary off ambient $PATH. Even with the
/// allowlist permitting the cwd, the program-trust gate fails the spawn.
#[test]
fn spawn_untrusted_child_program_is_rejected() {
    let db = TestDb::new();
    let log = std::path::PathBuf::from(format!("{}.tmuxlog", TestDb::new().path_str()));
    let _ = std::fs::remove_file(&log);
    let fake = make_fake_tmux_spawning(&log);

    let allow = std::path::PathBuf::from(format!("{}.allow", TestDb::new().path_str()));
    std::fs::create_dir_all(&allow).unwrap();
    let untrusted = std::path::PathBuf::from(format!("{}.untrusted", TestDb::new().path_str()));
    std::fs::create_dir_all(&untrusted).unwrap();
    let evil = untrusted.join("evil");
    std::fs::write(&evil, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&evil).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&evil, perms).unwrap();

    let (ok, _out, err) = run_with_fake_mux(
        &db,
        &fake,
        &[
            ("TMUX_PANE", "%1"),
            ("WEAVE_SPAWN_DIRS", allow.to_str().unwrap()),
        ],
        &[
            "spawn",
            "--name",
            "evil",
            "--mux",
            "tmux",
            "--cwd",
            allow.to_str().unwrap(),
            "--cmd",
            evil.to_str().unwrap(),
        ],
    );
    assert!(!ok, "an untrusted child program must be rejected");
    assert!(
        err.contains("trusted directory") || err.contains("not in a trusted"),
        "rejection should reference the trusted-dir gate: {err:?}"
    );
    let logged = read_mux_log(&log);
    assert!(
        !logged.contains("split-window"),
        "no launch with an untrusted child program:\n{logged}"
    );
}

/// An oversized child argv (more than `MAX_SPAWN_ARGS` elements) is rejected before
/// any launch. We pass a permitted cwd + trusted program but a flood of args.
#[test]
fn spawn_oversized_child_argv_is_rejected() {
    let db = TestDb::new();
    let log = std::path::PathBuf::from(format!("{}.tmuxlog", TestDb::new().path_str()));
    let _ = std::fs::remove_file(&log);
    let fake = make_fake_tmux_spawning(&log);

    let allow = std::path::PathBuf::from(format!("{}.allow", TestDb::new().path_str()));
    std::fs::create_dir_all(&allow).unwrap();

    // MAX_SPAWN_ARGS is 64; build well over it (echo + 200 args).
    let mut args: Vec<String> = vec![
        "spawn".into(),
        "--name".into(),
        "big".into(),
        "--mux".into(),
        "tmux".into(),
        "--cwd".into(),
        allow.to_string_lossy().into_owned(),
        "--cmd".into(),
        "echo".into(),
    ];
    for _ in 0..200 {
        args.push("x".into());
    }
    let argref: Vec<&str> = args.iter().map(String::as_str).collect();

    let (ok, _out, err) = run_with_fake_mux(
        &db,
        &fake,
        &[
            ("TMUX_PANE", "%1"),
            ("WEAVE_SPAWN_DIRS", allow.to_str().unwrap()),
        ],
        &argref,
    );
    assert!(!ok, "an oversized child argv must be rejected");
    assert!(
        err.to_lowercase().contains("args") || err.to_lowercase().contains("max"),
        "rejection should reference the argv cap: {err:?}"
    );
    let logged = read_mux_log(&log);
    assert!(
        !logged.contains("split-window"),
        "no launch with an oversized child argv:\n{logged}"
    );
}

/// Kill must never drive a mux with an attacker-influenced (id_valid-failing) target.
/// We attempt to register a tmux peer whose pane id carries a shell metacharacter.
/// Either registration rejects the hostile env id up front (already safe), or the
/// kill-time `id_valid` gate refuses — in NO case does the hostile id reach a kill
/// argv against the fake mux.
#[test]
fn kill_refuses_invalid_target_id() {
    let db = TestDb::new();
    let log = std::path::PathBuf::from(format!("{}.tmuxlog", TestDb::new().path_str()));
    let _ = std::fs::remove_file(&log);
    let fake = make_fake_tmux_spawning(&log);

    let (ok, _o, _e) = run_with_fake_mux(
        &db,
        &fake,
        &[("TMUX_PANE", "%1; rm -rf /")],
        &["register", "--name", "bad"],
    );
    // If registration rejected the hostile pane id, the bad id never persisted.
    if ok {
        // Otherwise the kill-time id_valid guard must refuse to drive the mux.
        let (_kok, kout, kerr) = run_with_fake_mux(&db, &fake, &[], &["kill", "--name", "bad"]);
        let combined = format!("{kout}{kerr}");
        assert!(
            combined.contains("invalid") || combined.contains("refusing"),
            "kill must refuse an invalid target: out={kout:?} err={kerr:?}"
        );
    }
    let logged = read_mux_log(&log);
    assert!(
        !logged.contains("rm -rf"),
        "a hostile target id must never reach the kill argv:\n{logged}"
    );
}

// ---------------------------------------------------------------------------
// WL-048: human surfaces — XSS end-to-end + bot-token secrecy (surfaces feature).
// ---------------------------------------------------------------------------

#[cfg(feature = "surfaces")]
mod surfaces_security {
    use super::{run_env, scrub_env, weave_bin, TestDb};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        l.local_addr().unwrap().port()
    }

    struct Dashboard {
        child: Child,
        port: u16,
    }
    impl Drop for Dashboard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn spawn_dashboard(db: &TestDb, token: &str) -> Dashboard {
        let port = free_port();
        let mut cmd = Command::new(weave_bin());
        cmd.args(["dashboard", "--port", &port.to_string(), "--token", token]);
        scrub_env(&mut cmd);
        cmd.env("WEAVE_DB", db.path_str());
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn weave dashboard");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                panic!("dashboard did not start on port {port}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Dashboard { child, port }
    }

    fn http_get(port: u16, path: &str, bearer: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bearer}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).expect("write");
        s.flush().ok();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// End-to-end XSS: a stored message body and a registered peer name containing
    /// `<script>` MUST be HTML-escaped in the served dashboard page — the raw tag
    /// must NOT appear, the escaped entity MUST.
    #[test]
    fn dashboard_escapes_stored_xss_payload_end_to_end() {
        let db = TestDb::new();
        let xss_name = "<script>alert(1)</script>";
        let xss_body = "<script>alert('pwn')</script>";
        // Register a peer whose NAME is the XSS payload (foreign host so it sticks).
        run_env(
            &db,
            &["register", "--name", xss_name],
            &[("HOSTNAME", "h2")],
        );
        run_env(&db, &["register", "--name", "bob"], &[("HOSTNAME", "h2")]);
        // Send a message whose BODY is the XSS payload.
        run_env(
            &db,
            &[
                "send", "--from", "bob", "--to", xss_name, "--body", xss_body,
            ],
            &[("HOSTNAME", "h2")],
        );

        let dash = spawn_dashboard(&db, "tok");
        let resp = http_get(dash.port, "/", "tok");
        assert!(resp.starts_with("HTTP/1.1 200"), "page should 200: {resp}");

        // The RAW <script> payloads must NOT survive into the response.
        assert!(
            !resp.contains("<script>alert(1)</script>"),
            "unescaped peer-name XSS leaked into the dashboard:\n{resp}"
        );
        assert!(
            !resp.contains("<script>alert('pwn')</script>"),
            "unescaped message-body XSS leaked into the dashboard:\n{resp}"
        );
        // The ESCAPED forms must be present (proof the payload was rendered, escaped).
        assert!(
            resp.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "escaped peer name missing — payload may not have rendered at all:\n{resp}"
        );
        assert!(
            resp.contains("&lt;script&gt;alert(&#x27;pwn&#x27;)&lt;/script&gt;"),
            "escaped message body missing — payload may not have rendered at all:\n{resp}"
        );
    }
}

// Bot-token secrecy is asserted directly against the bridge code (no live network)
// in the `weave-core` config Debug-redaction unit test and the `weave` bin's
// telegram/slack error-path unit tests — see `secret_redacted_in_debug` and
// `error_log_never_contains_token`.

// ---------------------------------------------------------------------------
// WL-049 / ADR-0002: governed web access hardening.
//
// All tests drive the real binary; none require (or contact) a real browser. The
// obscura child is either a fake stub or never spawned (policy refuses first).
// ---------------------------------------------------------------------------
#[cfg(feature = "obscura")]
mod obscura_security {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn make_fake_obscura() -> std::path::PathBuf {
        let dir = common::unique_db().with_extension("obscurabin-sec");
        std::fs::create_dir_all(&dir).expect("create fake-obscura dir");
        let script = dir.join("obscura");
        // Echoes the request id back; for tools/call it leaks a SECRET-looking
        // marker on stderr — the test asserts weave never surfaces the child stderr
        // (child output redaction; WL-048 lesson).
        let body = r#"#!/bin/sh
echo "OBSCURA-STDERR-SECRET-TOKEN abcdef" 1>&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"serverInfo":{"name":"obscura-mcp"}}}\n' "$id"
      ;;
    *'notifications/initialized'*) : ;;
    *'"tools/call"'*)
      echo "OBSCURA-STDERR-SECRET-TOKEN per-op" 1>&2
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}]}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"x"}}\n' "$id"
      ;;
  esac
done
"#;
        std::fs::write(&script, body).expect("write fake obscura");
        let mut perms = std::fs::metadata(&script)
            .expect("stat fake obscura")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod +x fake obscura");
        dir
    }

    fn web_cmd(db: &TestDb, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = common::weave_cmd(db, args);
        cmd.env("WEAVE_MUX_DIR", dir);
        cmd.env("WEAVE_OBSCURA_BIN", "obscura");
        cmd
    }

    /// A run helper: returns (ok, stdout, stderr).
    fn run_web(
        db: &TestDb,
        dir: &Path,
        web_args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> (bool, String, String) {
        let mut args = vec!["web"];
        args.extend_from_slice(web_args);
        let mut cmd = web_cmd(db, dir, &args);
        cmd.env("WEAVE_SESSION", "tester");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.stdin(Stdio::null()).output().expect("run weave web");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Deny-by-default holds under an adversarial action value.
    #[test]
    fn web_deny_by_default_under_adversarial_action() {
        let db = TestDb::new();
        let dir = make_fake_obscura();
        for action in [
            "navigate",
            "evaluate",
            "../../etc/passwd",
            "browser_navigate; rm -rf",
        ] {
            let (ok, _out, err) = run_web(&db, &dir, &[action], &[]);
            assert!(!ok, "deny-by-default must refuse {action:?}");
            assert!(
                err.contains("not allowed by policy") || err.contains("unknown web op"),
                "expected a policy/unknown refusal for {action:?}, got: {err}"
            );
        }
    }

    /// A shell-metacharacter / non-trusted obscura bin name is never interpreted as
    /// a shell command — it simply fails to resolve to a trusted binary (no spawn).
    #[test]
    fn web_obscura_bin_is_not_shell_interpreted() {
        let db = TestDb::new();
        let dir = make_fake_obscura();
        let mut cmd = common::weave_cmd(&db, &["web", "navigate", "--url", "https://example.com"]);
        cmd.env("WEAVE_MUX_DIR", dir);
        // A hostile "binary" that, if it ever reached a shell, would be a command.
        cmd.env("WEAVE_OBSCURA_BIN", "obscura; touch /tmp/weave-pwned");
        cmd.env("WEAVE_OBSCURA_ALLOW_OPS", "navigate");
        cmd.env("WEAVE_SESSION", "tester");
        let out = cmd.stdin(Stdio::null()).output().expect("run weave web");
        assert!(
            !out.status.success(),
            "a non-trusted/metachar obscura bin must not run"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("not found in a trusted directory"),
            "expected trusted-dir refusal, got: {err}"
        );
    }

    /// SSRF/localhost is blocked before any spawn.
    #[test]
    fn web_ssrf_localhost_blocked() {
        let db = TestDb::new();
        let dir = make_fake_obscura();
        for url in [
            "http://127.0.0.1",
            "http://localhost:9000/admin",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
        ] {
            let (ok, _out, err) = run_web(
                &db,
                &dir,
                &["navigate", "--url", url],
                &[("WEAVE_OBSCURA_ALLOW_OPS", "navigate")],
            );
            assert!(!ok, "{url} must be refused");
            assert!(
                err.contains("SSRF guard"),
                "expected SSRF refusal for {url}, got: {err}"
            );
        }
    }

    /// The obscura child's stderr (a leaked secret) must NEVER appear in weave's
    /// own stdout or stderr.
    #[test]
    fn web_child_stderr_not_leaked() {
        let db = TestDb::new();
        let dir = make_fake_obscura();
        let (ok, out, err) = run_web(
            &db,
            &dir,
            &["navigate", "--url", "https://example.com"],
            &[("WEAVE_OBSCURA_ALLOW_OPS", "navigate")],
        );
        assert!(ok, "navigate should succeed.\nstdout: {out}\nstderr: {err}");
        assert!(
            !out.contains("OBSCURA-STDERR-SECRET-TOKEN"),
            "child stderr secret leaked into weave stdout: {out}"
        );
        assert!(
            !err.contains("OBSCURA-STDERR-SECRET-TOKEN"),
            "child stderr secret leaked into weave stderr: {err}"
        );
    }

    /// MCP stdout discipline: `weave_web` driven over the real `weave mcp` stdio
    /// server returns ONLY a clean JSON-RPC result frame — the obscura child's
    /// stdout/stderr noise must never bleed into weave's own protocol stream. The
    /// `McpServer` harness panics on any non-JSON stdout line, so a single parseable
    /// frame with the canned text (and no child secret) is the discipline proof.
    #[test]
    fn web_over_mcp_stdout_is_pure_jsonrpc() {
        let db = TestDb::new();
        let dir = make_fake_obscura();
        let dir_s = dir.to_string_lossy().into_owned();
        let mut mcp = McpServer::spawn_env(
            &db,
            &[
                ("WEAVE_MUX_DIR", dir_s.as_str()),
                ("WEAVE_OBSCURA_BIN", "obscura"),
                ("WEAVE_OBSCURA_ALLOW_OPS", "navigate"),
                ("WEAVE_SESSION", "tester"),
            ],
        );
        let (is_err, text) = mcp.call_tool(
            "weave_web",
            serde_json::json!({
                "me": "tester",
                "action": "navigate",
                "args": {"url": "https://example.com"}
            }),
        );
        // The fake stub leaks OBSCURA-STDERR-SECRET-TOKEN on its stderr; weave must
        // surface only the canned content text, in a single clean result frame.
        assert!(!is_err, "navigate via MCP should succeed: {text}");
        assert_eq!(
            text, "ok",
            "expected the canned obscura payload, got: {text:?}"
        );
        assert!(
            !text.contains("OBSCURA-STDERR-SECRET-TOKEN"),
            "child stderr secret leaked into the MCP result frame: {text}"
        );
        mcp.shutdown();
    }
}
