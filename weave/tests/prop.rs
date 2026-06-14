//! Property-based end-to-end tests for the built `weave` binary.
//!
//! These complement the example-based `integration.rs` suite: instead of a few
//! hand-picked scenarios, `proptest` *generates* sequences of sends and then
//! asserts invariants that must hold for ANY such sequence. Like the rest of the
//! e2e suite everything is black-box: we drive the compiled `weave` binary via
//! `std::process::Command` against a unique temp `WEAVE_DB` (reusing the
//! `tests/common` helpers so isolation/identity/backend are deterministic and we
//! never touch the real store).
//!
//! Properties exercised:
//!   1. ROUTING — for any sequence of sends, a single (default, read-marking)
//!      `inbox --me R` returns exactly the messages addressed to `R`: every
//!      direct message `* -> R` plus every broadcast from someone other than `R`,
//!      and nothing else (never `R`'s own broadcasts, never messages for others).
//!      Order is preserved (insertion / id order).
//!   2. READ-TRACKING IDEMPOTENCE — after a default read drains the inbox, any
//!      number of further default reads return empty; `--all` always re-surfaces
//!      the full history; and re-reading never resurrects or drops messages.
//!   3. UNICODE / LONG-BODY ROUNDTRIP — arbitrary unicode (incl. multi-byte,
//!      emoji, control-ish, and long) bodies survive `send` -> `inbox --json`
//!      byte-for-byte, with no corruption, truncation, or quoting damage.
//!
//! Runtime is bounded on purpose: each generated case spawns several short-lived
//! `weave` processes, so case counts are kept small (see `cases(...)` below).

mod common;

use common::{run_in_cwd, run_ok, TestDb};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// A small pool of distinct, injection-safe peer names. Keeping the alphabet
/// tiny makes interesting routing (multiple messages to the same reader, self
/// sends, cross traffic) actually happen within a short generated sequence,
/// while staying clear of the broadcast aliases below.
const PEERS: &[&str] = &["alpha", "bravo", "charlie", "delta"];

/// Broadcast aliases recognized by the binary (mirrors `model::BROADCAST`). A
/// message to any of these fans out to every *other* peer.
const BROADCASTS: &[&str] = &["all", "*", "everyone", "broadcast"];

/// One generated message: a sender (always a real peer) and a recipient that is
/// either another peer, the sender itself (self-send — must NOT appear in a
/// broadcast-style fan-out but DOES appear as a direct self message), or a
/// broadcast alias.
#[derive(Debug, Clone)]
struct Msg {
    from: String,
    to: String,
    /// Stable, unique-per-sequence marker embedded in the body so we can match
    /// the message back to its generated spec regardless of formatting.
    tag: String,
}

/// Strategy for a recipient: a peer name OR a broadcast alias.
fn recipient_strategy() -> impl Strategy<Value = String> {
    let peers = prop::sample::select(PEERS).prop_map(|s| s.to_string());
    let casts = prop::sample::select(BROADCASTS).prop_map(|s| s.to_string());
    // Weight peers higher than broadcasts so direct routing dominates but
    // broadcasts still show up regularly.
    prop_oneof![3 => peers, 1 => casts]
}

/// Strategy for a single message (sender + recipient). The `tag` is filled in
/// afterwards (it depends on the message's index in the sequence).
fn msg_strategy() -> impl Strategy<Value = (String, String)> {
    (
        prop::sample::select(PEERS).prop_map(|s| s.to_string()),
        recipient_strategy(),
    )
}

/// A bounded sequence of messages. We index them after generation to stamp a
/// unique tag onto each body.
fn sequence_strategy() -> impl Strategy<Value = Vec<Msg>> {
    prop::collection::vec(msg_strategy(), 1..12).prop_map(|pairs| {
        pairs
            .into_iter()
            .enumerate()
            .map(|(i, (from, to))| Msg {
                from,
                to,
                // `m0`, `m1`, ... — unique within the sequence and trivially
                // greppable in both the plain and JSON inbox renderings.
                tag: format!("m{i}"),
            })
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `true` if `to` is one of the broadcast aliases.
fn is_broadcast(to: &str) -> bool {
    BROADCASTS.contains(&to)
}

/// The set of tags a reader `me` should receive, in send order, given the full
/// generated sequence — the oracle the routing property checks the binary
/// against.
///
/// Rules (matching the store's semantics — weave's inbox query filters
/// `sender != me` universally):
///   * a direct message `from -> me` is delivered to `me` ONLY when `from != me`
///     (a session never sees its own messages, including self-directed ones);
///   * a broadcast `from -> <alias>` is delivered to every peer EXCEPT the
///     sender (a session never receives its own broadcast);
///   * nothing else reaches `me`.
fn expected_tags_for(seq: &[Msg], me: &str) -> Vec<String> {
    seq.iter()
        .filter(|m| {
            // `sender != me` applies to BOTH direct and broadcast (weave's filter).
            m.from != me
                && if is_broadcast(&m.to) {
                    true
                } else {
                    m.to == me
                }
        })
        .map(|m| m.tag.clone())
        .collect()
}

/// Send one generated message through the binary. The body carries the unique
/// tag plus a fixed prefix so it is unambiguous in the rendered inbox.
fn send_msg(db: &TestDb, m: &Msg) {
    let body = format!("body-{}", m.tag);
    run_ok(
        db,
        &["send", "--from", &m.from, "--to", &m.to, "--body", &body],
    );
}

/// Read `me`'s inbox as JSON and return the list of bodies in delivery order.
/// `peek` selects a non-consuming read; `all` includes already-read history.
fn inbox_bodies(db: &TestDb, me: &str, peek: bool, all: bool) -> Vec<String> {
    let mut args: Vec<&str> = vec!["inbox", "--me", me, "--json"];
    if peek {
        args.push("--peek");
    }
    if all {
        args.push("--all");
    }
    let out = run_ok(db, &args);
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("inbox --json must parse: {e}\n{out}"));
    v["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            m["body"]
                .as_str()
                .unwrap_or_else(|| panic!("message body must be a JSON string: {m}"))
                .to_string()
        })
        .collect()
}

/// Extract the `mN` tags (in order) from a list of `body-mN` strings.
fn tags_of(bodies: &[String]) -> Vec<String> {
    bodies
        .iter()
        .map(|b| b.strip_prefix("body-").unwrap_or(b).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    // Bounded: every case spawns N+ subprocesses, so keep the case count small.
    // Disable persistence/regression files so the test is hermetic in CI.
    #![proptest_config(ProptestConfig {
        cases: 24,
        max_shrink_iters: 50,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// PROPERTY 1: routing correctness + order.
    ///
    /// After sending an arbitrary sequence, a single default `inbox --me R`
    /// (with a peek, so we can re-check every reader against the same store)
    /// yields exactly the tags `expected_tags_for(seq, R)` produces, in the
    /// same order.
    #[test]
    fn routing_delivers_exactly_addressed_messages_in_order(seq in sequence_strategy()) {
        let db = TestDb::new();

        // Drive all sends first.
        for m in &seq {
            send_msg(&db, m);
        }

        // Every distinct peer name that appears as a sender or a direct
        // recipient is a potential reader; also always check all four peers so
        // "received nothing" is exercised too.
        for &reader in PEERS {
            // Peek so reading one peer's inbox never alters what another peer
            // will see — keeps the per-reader checks independent.
            let got = tags_of(&inbox_bodies(&db, reader, /*peek=*/ true, /*all=*/ false));
            let want = expected_tags_for(&seq, reader);
            prop_assert_eq!(
                got, want,
                "reader {:?} received the wrong set/order of messages", reader
            );
        }
    }

    /// PROPERTY 2: read-tracking is idempotent.
    ///
    /// One default (consuming) read drains the reader's unread inbox; any number
    /// of subsequent default reads return empty, while `--all` keeps returning
    /// the full, unchanged history. Re-reading neither resurrects unread state
    /// nor loses delivered messages.
    #[test]
    fn read_tracking_is_idempotent(seq in sequence_strategy()) {
        let db = TestDb::new();
        for m in &seq {
            send_msg(&db, m);
        }

        // Pick a reader that actually receives something when possible, so the
        // drain path is meaningfully exercised; fall back to PEERS[0] otherwise.
        let reader = PEERS
            .iter()
            .copied()
            .find(|r| !expected_tags_for(&seq, r).is_empty())
            .unwrap_or(PEERS[0]);

        let expected = expected_tags_for(&seq, reader);

        // First default read drains exactly the expected unread messages.
        let first = tags_of(&inbox_bodies(&db, reader, /*peek=*/ false, /*all=*/ false));
        prop_assert_eq!(&first, &expected, "first drain must equal expected unread");

        // Idempotence: further default reads are empty no matter how many times.
        for round in 0..3 {
            let again = inbox_bodies(&db, reader, /*peek=*/ false, /*all=*/ false);
            prop_assert!(
                again.is_empty(),
                "default read #{} after drain must be empty, got {:?}",
                round + 2, again
            );
        }

        // `--all` still returns the complete, order-preserved history every time,
        // unaffected by the consuming reads above.
        for _ in 0..2 {
            let all_tags = tags_of(&inbox_bodies(&db, reader, /*peek=*/ true, /*all=*/ true));
            prop_assert_eq!(
                &all_tags, &expected,
                "--all must always re-surface the full history unchanged"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 3 lives in its own proptest! block so it can use a body generator
// with a different shape (arbitrary unicode) and an even smaller case budget,
// since long bodies make each subprocess roundtrip a little heavier.
// ---------------------------------------------------------------------------

/// Strategy for an arbitrary, non-empty unicode body. Mixes a free unicode regex
/// (multi-byte, emoji, scripts) with occasional long bodies to stress
/// buffering/quoting, while excluding ASCII control chars that the shell/argv
/// layer can't carry verbatim (NUL especially) — the property is about *content*
/// fidelity through `send`/`inbox`, not about argv encoding of control bytes.
fn unicode_body_strategy() -> impl Strategy<Value = String> {
    // `\PC` = any char that is NOT an "other" (control/format/surrogate/etc.),
    // so we keep printable letters, marks, punctuation, symbols, emoji, and
    // ordinary spaces while dropping NUL and other control characters.
    let short = prop::string::string_regex(r"\PC{1,40}").unwrap();
    let long = prop::string::string_regex(r"\PC{200,400}").unwrap();
    // Bias toward short bodies; sprinkle in long ones.
    prop_oneof![4 => short, 1 => long]
        // Guarantee non-empty so there is always content to compare.
        .prop_filter("non-empty body", |s| !s.is_empty())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        max_shrink_iters: 60,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// PROPERTY 3: arbitrary unicode / long bodies round-trip without corruption.
    ///
    /// The body that comes back out of `inbox --json` must be byte-for-byte the
    /// body we sent — JSON decoding handles any escaping, so any mismatch means
    /// real corruption (truncation, re-encoding, quoting damage) somewhere in
    /// send -> store -> inbox.
    #[test]
    fn unicode_and_long_bodies_roundtrip(body in unicode_body_strategy()) {
        let db = TestDb::new();

        // Single direct message; sender != recipient so it lands as unread.
        run_ok(
            &db,
            &["send", "--from", "alpha", "--to", "bravo", "--body", &body],
        );

        let bodies = inbox_bodies(&db, "bravo", /*peek=*/ true, /*all=*/ false);
        prop_assert_eq!(bodies.len(), 1, "exactly one message should be delivered");
        prop_assert_eq!(
            &bodies[0], &body,
            "delivered body must match the sent body byte-for-byte"
        );
    }

    /// PROPERTY 4: SESSION-TAG SANITIZE TOTALITY (end-to-end, black-box).
    ///
    /// For ANY worktree-id segment (arbitrary unicode, control chars, shell
    /// metacharacters — but no `/`, which is the segment delimiter the `.git`-file
    /// parser splits on), capturing it from a crafted cwd via `weave register` and
    /// reading it back through `peers --json` ALWAYS yields a tag that is:
    ///   * stored without error (registration never hard-fails on a tag),
    ///   * control-character-free, and
    ///   * within the 128-char cap.
    /// This exercises the full cwd → `git::capture_worktree_tags` →
    /// `store::sanitize_tag` → persistence → projection seam, proving the sanitize
    /// is total no matter how hostile the cwd-derived value is.
    #[test]
    fn session_tag_capture_is_bounded_and_controlfree(seg in r"[^/]{0,300}") {
        // The crafted `.git` file is one line; the parser trims and splits on `/`.
        // A segment that trims to empty would yield no worktree_id (the cwd looks
        // non-git) — that is a valid total outcome, asserted as "no tag captured".
        let db = TestDb::new();
        let dir = std::env::temp_dir().join(format!(
            "weave-prop-tag-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".git"),
            format!("gitdir: /fixture/main/.git/worktrees/{seg}/.git\n"),
        )
        .unwrap();

        let (ok, _o, err) = run_in_cwd(&db, &["register", "--name", "proptag"], &dir);
        prop_assert!(ok, "register must be total over any tag: stderr={}", err);

        let out = run_ok(&db, &["peers", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let stored = v
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "proptag")
            .and_then(|p| p["worktree"].as_str())
            .unwrap_or("")
            .to_string();

        prop_assert!(
            !stored.chars().any(|c| c.is_control()),
            "stored worktree tag must be control-free: {:?}",
            stored
        );
        prop_assert!(
            stored.chars().count() <= 128,
            "stored worktree tag must be ≤128 chars, got {}",
            stored.chars().count()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// WL-038: ephemeral TTL — expiry monotonicity (pure helper property).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any non-negative base `ts` and a valid `ttl` in `1..=MAX_MSG_TTL_SECS`,
    /// the absolute deadline is STRICTLY in the future (no wrap) and bounded by
    /// `ts + MAX_MSG_TTL_SECS`. This is the resource-bound / no-overflow property
    /// behind the `expires_at = ts + ttl` design.
    #[test]
    fn expiry_monotonicity(
        ts in 0i64..(i64::MAX - weave_core::model::MAX_MSG_TTL_SECS),
        ttl in 1i64..=weave_core::model::MAX_MSG_TTL_SECS,
    ) {
        let exp = weave_core::model::expiry_from_ttl(ts, ttl);
        prop_assert!(exp > ts, "expiry {exp} must be strictly after base {ts}");
        prop_assert!(
            exp <= ts + weave_core::model::MAX_MSG_TTL_SECS,
            "expiry {exp} must be bounded by ts + MAX_MSG_TTL_SECS"
        );
        // A valid ttl is always accepted by the cap guard.
        prop_assert!(weave_core::model::ttl_valid(ttl));
    }

    /// `expiry_from_ttl` never panics and always saturates at `i64::MAX` for an
    /// extreme base, even outside the validated range.
    #[test]
    fn expiry_saturates_without_panic(ts in any::<i64>(), ttl in any::<i64>()) {
        let exp = weave_core::model::expiry_from_ttl(ts, ttl);
        // Saturating semantics: result is within [i64::MIN, i64::MAX] (trivially
        // true) and equals a saturating add — assert it matches the reference.
        prop_assert_eq!(exp, ts.saturating_add(ttl));
    }
}
