//! Black-box throughput benchmarks for the `weave` binary.
//!
//! The store is a private module, so we cannot bench it in-process. Instead we
//! treat the *built* binary as the unit under test and drive it through
//! `std::process::Command`, exactly like the end-to-end integration tests. This
//! captures the cost that matters operationally: process cold-start + sqlite
//! open + the CLI roundtrip a hook/agent actually pays.
//!
//! Groups. `cold_start` times `weave --version` (clap parse only, no store open)
//! and `weave doctor --json` (opens the store, lists peers). `send_inbox` times a
//! `send` then `inbox --json` roundtrip against a fresh temp DB (full write + read
//! path through the binary). `inbox_json_parse` times a pure, in-process
//! `serde_json` parse of a large `inbox --json` payload (no subprocess in the hot
//! loop).
//!
//! Every child runs with a scrubbed environment and its own temp `WEAVE_DB`, so
//! the benches are deterministic, parallel-safe, and never touch the real store.
//! Process-spawning benches are inherently noisy; criterion's outlier handling
//! plus a modest sample size keep them stable enough to track regressions.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Test-harness plumbing (mirrors tests/common, kept standalone so the bench
// crate has no dependency on the test crate's module tree).
// ---------------------------------------------------------------------------

/// Absolute path to the freshly built `weave` binary. Cargo exports
/// `CARGO_BIN_EXE_<name>` to integration tests, benches, and examples alike.
fn weave_bin() -> &'static str {
    env!("CARGO_BIN_EXE_weave")
}

/// A unique temp DB path. Combines pid + a monotonic counter + nanos so parallel
/// runs never collide. The sqlite backend creates the file lazily on open.
fn unique_db() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("weave-bench-{pid}-{n}-{nanos}.db"))
}

/// A temp DB that removes its sqlite files (and sidecars) on drop.
struct BenchDb {
    path: PathBuf,
}

impl BenchDb {
    fn new() -> Self {
        BenchDb { path: unique_db() }
    }

    fn path_str(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for BenchDb {
    fn drop(&mut self) {
        let base = self.path.to_string_lossy().into_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{base}{suffix}"));
        }
    }
}

/// Strip every env var that could make a child non-deterministic, then point
/// config discovery at an empty dir so no real `config.toml` is read. Identical
/// in spirit to `tests/common::scrub_env`.
fn scrub_env(cmd: &mut Command) {
    for k in [
        "WEAVE_SESSION",
        "WEAVE_BACKEND",
        "WEAVE_DB",
        "WEAVE_LIBSQL_URL",
        "WEAVE_LIBSQL_AUTH_TOKEN",
        "TMUX_PANE",
        "ZELLIJ_SESSION_NAME",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "STY",
        "XDG_CONFIG_HOME",
    ] {
        cmd.env_remove(k);
    }
    cmd.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("weave-bench-noconfig"),
    );
}

/// Build a `weave <args...>` command pinned to `db` with a scrubbed environment.
fn weave_cmd(db: &BenchDb, args: &[&str]) -> Command {
    let mut cmd = Command::new(weave_bin());
    cmd.args(args);
    scrub_env(&mut cmd);
    cmd.env("WEAVE_DB", db.path_str());
    cmd
}

/// Run a `weave` subcommand to completion, asserting success, and return stdout.
/// stdin is nulled so commands that would otherwise read it never block.
fn run_ok(db: &BenchDb, args: &[&str]) -> String {
    let out = weave_cmd(db, args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn weave {args:?}: {e}"));
    assert!(
        out.status.success(),
        "`weave {args:?}` exited non-zero\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Cold-start cost of the binary. Two flavors:
///   * `--version` — clap parses and prints; the store is never opened. This is
///     the pure process-spawn + arg-parse floor.
///   * `doctor --json` — opens the sqlite store, lists peers, serializes JSON.
///     The delta over `--version` is roughly the store-open + query cost.
fn bench_cold_start(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_start");
    // Process spawns are slow and noisy; a smaller sample keeps wall time sane.
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(12));

    group.bench_function("version", |b| {
        // No store needed; reuse one scrubbed command shape per iteration.
        let db = BenchDb::new();
        b.iter(|| {
            let out = run_ok(&db, &["--version"]);
            black_box(out);
        });
    });

    group.bench_function("doctor_json", |b| {
        // A fresh DB per benchmark (not per-iteration): doctor only reads, so the
        // store can be reused across iterations and we still measure open+query.
        let db = BenchDb::new();
        b.iter(|| {
            let out = run_ok(&db, &["doctor", "--json"]);
            black_box(out);
        });
    });

    group.finish();
}

/// Full send -> inbox roundtrip through the binary: one process writes a message
/// (sqlite INSERT), a second process drains the inbox as JSON (SELECT + mark
/// read + serialize). A fresh DB per iteration keeps inbox size constant (one
/// unread message) so the measurement does not drift as rows accumulate.
fn bench_send_inbox(c: &mut Criterion) {
    let mut group = c.benchmark_group("send_inbox");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));
    group.throughput(Throughput::Elements(1)); // one message per roundtrip

    group.bench_function("send_then_inbox_json", |b| {
        b.iter_batched(
            // Setup (untimed): a pristine DB for each roundtrip.
            BenchDb::new,
            // Routine (timed): the two-process roundtrip.
            |db| {
                let sent = run_ok(
                    &db,
                    &[
                        "send",
                        "--from",
                        "alice",
                        "--to",
                        "bob",
                        "--subject",
                        "ping",
                        "--body",
                        "hello from the throughput benchmark",
                    ],
                );
                black_box(sent);
                let inbox = run_ok(&db, &["inbox", "--me", "bob", "--json"]);
                black_box(inbox);
            },
            // Each iteration mutates the DB (sends + marks read), so it cannot be
            // reused; recreate per iteration.
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

/// Build a realistic `inbox --json` payload (the shape main.rs prints for
/// `weave inbox --json`: `{me, messages:[...], remaining_unread}`) with `n`
/// messages. Used to feed the pure-parse benchmark below.
fn synthetic_inbox_json(n: usize) -> String {
    let mut messages = String::with_capacity(n * 160);
    messages.push('[');
    for i in 0..n {
        if i > 0 {
            messages.push(',');
        }
        // Mirror the serde shape of model::Message exactly: id, ts, sender,
        // recipient, subject (nullable), body.
        let subject = if i % 3 == 0 {
            "null".to_string()
        } else {
            format!("\"subject line {i}\"")
        };
        messages.push_str(&format!(
            "{{\"id\":{id},\"ts\":{ts},\"sender\":\"agent-{s}\",\"recipient\":\"bob\",\
             \"subject\":{subject},\"body\":\"message body number {i} with some padding \
             so each record has a realistic on-the-wire size for parsing\"}}",
            id = i + 1,
            ts = 1_700_000_000 + i as i64,
            s = i % 7,
        ));
    }
    messages.push(']');
    format!("{{\"me\":\"bob\",\"messages\":{messages},\"remaining_unread\":0}}")
}

/// Pure, in-process `serde_json` parse of a large `inbox --json` document. No
/// subprocess in the hot loop — this isolates deserialization throughput, which
/// is the cost an agent pays when it consumes a big inbox dump. We parse into a
/// generic `Value` so the bench needs no access to weave's private types.
fn bench_inbox_json_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("inbox_json_parse");

    for &n in &[100usize, 1_000, 10_000] {
        let payload = synthetic_inbox_json(n);
        // Throughput in bytes lets criterion report MiB/s for the parser.
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_function(format!("messages_{n}"), |b| {
            b.iter(|| {
                let v: serde_json::Value =
                    serde_json::from_str(black_box(&payload)).expect("parse inbox json");
                // Touch the parsed structure so the optimizer can't elide the work.
                let count = v
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                black_box(count);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_cold_start,
    bench_send_inbox,
    bench_inbox_json_parse,
);
criterion_main!(benches);
