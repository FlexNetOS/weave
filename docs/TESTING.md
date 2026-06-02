# Testing & Verification Strategy

weave is a single static binary that other agents trust with their messages and,
through the injector, with *typing into a live terminal pane*. That trust budget
is what the test suite is built to protect. Every layer that can be exercised
in-process is unit-tested; every layer that only exists once the binary is
assembled (arg parsing, the MCP stdio protocol, the injector reaching a real
mux) is exercised black-box against the compiled binary; and the routing /
read-tracking / body-fidelity invariants that must hold for *any* input are
checked with property-based tests.

This document describes what is tested, how the suite runs on **both** storage
backends, the fake-mux harness, what proptest caught, and a checklist for adding
tests alongside a new feature.

## Test layers at a glance

| Layer | Location | Style | What it pins down |
|-------|----------|-------|-------------------|
| Unit — pure logic | `src/*.rs` `#[cfg(test)]` modules | in-process | store semantics, injector argv shaping, model/broadcast invariants, config template, hook-command matcher |
| Integration — end-to-end | `tests/integration.rs` | black-box binary | MCP stdio protocol, CLI roundtrips, lifecycle hooks, `--json`/`doctor`/`gc`, backend selection |
| Security / hardening | `tests/security.rs` | black-box binary | flag-injection resistance, destructive-op guard, resource caps, at-rest file mode |
| Property-based | `tests/prop.rs` | black-box + `proptest` | routing correctness, read-tracking idempotence, unicode/long-body roundtrip |
| Benchmark | `benches/weave_bench.rs` | `criterion`, black-box | cold-start, send→inbox roundtrip, inbox-JSON parse throughput |

Run everything (default sqlite backend):

```bash
cargo test --all-targets
```

CI additionally gates `cargo fmt --all --check` and
`cargo clippy --all-targets -- -D warnings` (see `.github/workflows/ci.yml`).

## 1. Unit tests (in-process)

These live next to the code they cover and need no subprocess. They are the
fastest layer and carry most of the injector and store coverage.

- **`src/store.rs`** (`#[cfg(all(test, feature = "sqlite"))]`) — the bundled
  `SqliteStore` against a real on-disk temp DB: send + per-reader read tracking,
  peer upsert/presence (`is_online` / `ONLINE_TTL_SECS`), scoped history,
  `clamp_limit` bounds (a *negative* limit must not become SQLite's unbounded
  `LIMIT -1`), `gc` (deletes old, keeps new), reply auto-addressing + `Re:`
  prefixing + `in_reply_to` linking, transitive `thread` collection, read
  `receipts`, `touch_peer` (refresh without clobbering mux/target/cwd), and the
  `0600` DB file mode. Because these touch backend internals (`s.conn.execute`,
  the concrete `SqliteStore`), the module is gated to the `sqlite` feature — see
  the dual-backend note below for how the *same* behaviors are still verified
  under libSQL.

- **`src/inject.rs`** (23 tests) — `commands_for` / `commands_for_mode` are
  **pure** functions returning the exact argv table for each mux, so the entire
  injector shaping layer is asserted byte-for-byte without ever launching a
  terminal: tmux bracketed-paste close (`ESC[201~`) before Enter, zellij CR,
  kitty `--match id:` (and `--to <socket>` ordering when `KITTY_LISTEN_ON` is
  set), wezterm `--no-paste`, screen `stuff`. Plus the hardening rules:
  leading-dash bodies land as content behind an end-of-options `--`; empty /
  whitespace-only text injects nothing (no stray Enter); interior newlines
  collapse to one line; control chars are stripped; oversized bodies cap at
  `MAX_INJECT_CHARS` (240) on a UTF-8 boundary with a `…` marker; `Nudge::Nudge`
  never leaks the body; liveness probes shape per backend and are fail-open for
  unprobed backends; and **`id_valid` rejects malicious target ids**
  (`%3; rm -rf /`, `--listen-on=evil`, embedded spaces) — the injector
  target-validation guard.

- **`src/model.rs`** — the broadcast alias set is exposed as both a Rust check
  (`is_broadcast`) and a SQL literal (`BROADCAST_SQL`). A drift guard asserts the
  hand-maintained literal stays byte-identical to the fragment derived from
  `BROADCAST`, so the Rust path and the `recipient IN (...)` delivery filter can
  never disagree.

- **`src/config.rs`** — the `config.toml` scaffold parses as valid (all-default)
  TOML, documents every nudge placeholder, and mentions every real config field
  so a newly-added field can't be left undocumented.

- **`src/setup.rs`** — `is_weave_command` matches only real installed
  `weave hook <event>` lines (and absolute-path forms) while rejecting
  look-alikes (`myweave …`, `weave mcp`, an un-installed event), so
  `setup`/`uninstall` only touch weave's own `settings.json` entries.

## 2. Integration tests — black-box binary (`tests/integration.rs`)

Everything here drives the **built** binary through `std::process::Command`; the
path is resolved at compile time via `CARGO_BIN_EXE_weave`. Each test gets its
own unique temp `WEAVE_DB` (pid + monotonic counter + nanos) and a *scrubbed*
environment — every `WEAVE_*` and mux-detection var (`TMUX_PANE`,
`ZELLIJ_SESSION_NAME`, `WEZTERM_PANE`, `KITTY_WINDOW_ID`, `STY`) is removed and
`XDG_CONFIG_HOME` is pointed at an empty dir — so tests are isolated,
parallel-safe, deterministic, and never read the real store or config. The
`tests/common` module provides these helpers plus a small `McpServer` driver.

Coverage:

- **MCP stdio protocol.** A live `weave mcp` child is spoken to over
  newline-delimited JSON-RPC 2.0: `initialize` returns `serverInfo.name`,
  `notifications/initialized` gets *no* reply, `tools/list` advertises
  `weave_send`/`weave_inbox`/`weave_peers`, a `weave_send`→`weave_inbox`
  roundtrip proves delivery + read tracking, an unknown method returns a proper
  JSON-RPC error object, and closing stdin exits the server cleanly. Reads run on
  a background thread behind a 10s timeout so a wedged binary fails the test
  instead of hanging the suite.

- **CLI roundtrips.** `send`→`inbox` (body delivered, default read consumes, an
  unrelated recipient sees nothing), `register`→`peers` (registered outside a mux
  is `no-inject`), `sessions` reports unread counts, and the
  reply→thread→receipts roundtrip.

- **Lifecycle hooks** — the Claude Code integration. `hook session` registers a
  peer from the payload cwd basename; `hook prompt` drains *and* marks read with
  an explicit payload identity; `hook stop` is a non-consuming **peek** (two
  stops re-surface the same message); a *guessed* identity (empty stdin) peeks
  only and warns; and garbage / unknown events are tolerated (exit 0 with a
  stderr warning, not a crash).

- **New CLI surface.** `--json` output for `inbox`/`peers`/`sessions` parses and
  carries the right shape; `doctor --json` reports the compiled-in backend and db
  path; `gc --older-than-secs` reports a count; an unknown `WEAVE_BACKEND` fails
  loudly instead of silently defaulting; an unknown subcommand exits non-zero
  with a clap usage message.

## 3. Security tests (`tests/security.rs`)

Same black-box harness, focused on the properties that make weave safe to run
between semi-trusted agent sessions:

- **Flag-injection resistance.** A body that *looks* like a CLI flag
  (`--to=victim --body=pwned; rm -rf /`, or `-n -e --peek …`) is delivered
  byte-for-byte and never re-interpreted as an option — neither on the way in
  (clap parse; bodies use `allow_hyphen_values`) nor on the way out (inbox
  render). The assertion compares the exact stored bytes via `inbox --json`.

- **Destructive-op guard.** `weave_clear {scope:"all"}` without `confirm` returns
  an `isError` result *and* leaves the pre-existing message readable — a stray
  call can't wipe the mesh.

- **Resource guards.** A 100k-char identity is rejected by the MCP layer
  (`MAX_IDENT_LEN`) with `isError` rather than persisted; an over-`MAX_BODY`
  (>65536-byte) body is rejected at the store layer (shared by CLI/MCP/hook) with
  a clear "too long" error.

- **At-rest secrecy.** After a send creates the DB, `mode & 0o077 == 0` — message
  bodies never leak to other local users.

## 4. Property-based tests (`tests/prop.rs`)

proptest *generates* sequences of sends (a small fixed peer pool + broadcast
aliases, biased toward direct routing) and asserts invariants over the black-box
binary. Case counts are kept small (24–32) because each generated case spawns
several short-lived `weave` processes; `failure_persistence` is disabled so the
suite is hermetic in CI.

- **Property 1 — routing.** For any send sequence, a single `inbox --me R`
  returns *exactly* the messages addressed to `R` (every `* -> R` plus every
  broadcast from someone other than `R`, never `R`'s own broadcast, nothing for
  others) in send order — checked against an in-test oracle for all peers,
  including "received nothing".

- **Property 2 — read-tracking idempotence.** One default read drains the unread
  inbox; any number of further default reads are empty; `--all` always
  re-surfaces the full, order-preserved history; re-reading never resurrects or
  drops messages.

- **Property 3 — unicode / long-body roundtrip.** Arbitrary unicode (multi-byte,
  emoji, scripts) and long (200–400 char) bodies survive
  `send → store → inbox --json` byte-for-byte — JSON decoding handles escaping,
  so any mismatch is real corruption.

### What proptest caught: the leading-hyphen bug

The unicode/flag-shaped generators surfaced a real defect: a body that *started*
with a dash (e.g. `-n…` or `--…`) was being eaten by clap as an unknown flag
instead of delivered as data, so `send`/`reply` failed (or mis-parsed) on
perfectly valid content. The fix is `#[arg(long, allow_hyphen_values = true)]` on
the `body` (and `subject`) arguments in `src/main.rs`; the regression is now
locked down both by the property test and by the explicit verbatim-delivery cases
in `tests/security.rs`. (The mirror concern at the *output* end — a leading-dash
body re-parsed as a flag by the backend mux CLI — is guarded by the injector's
end-of-options `--` and its `leading_dash_body_is_content_not_a_flag` unit test.)

## 5. The fake-mux injector harness

The injector's argv shaping is proven by pure unit tests, but the
"does the binary actually *spawn* the mux and type the body into the right pane?"
path can only be verified end-to-end. `tests/integration.rs` does this **without
a real terminal** by planting a fake `tmux` on `PATH`:

`make_fake_tmux` writes an executable shell script named `tmux` into a temp dir.
The script records its full argv (`"$*"`) to a log file and exits 0:

```sh
#!/bin/sh
printf '%s\n' "$*" >> '<logfile>'
exit 0
```

The test prepends that dir to `PATH`, registers a peer while pretending to live
in tmux pane `%1` (`TMUX_PANE=%1`), then drives:

- `weave send --to p …` — and asserts the log shows a `send-keys -t %1` carrying
  the body, proving the real send path invoked the injector against the right
  pane; and
- `weave inject --to p --text hi` — the explicit-inject fallback, asserting
  `send-keys -t %7 … hi`.

The log is read with a short bounded backoff (`read_log_with_retries`, ~50×20ms)
so the asynchronous write never flakes and never hangs. Because the fake mux only
has to *exist* on `PATH` for `have()`/`peers` to call the target injectable, this
harness also exercises the "injectable tmux peer" listing.

## 6. Dual-backend testing (sqlite **and** libSQL)

weave ships two mutually-exclusive storage backends — bundled `rusqlite`
(default) and `libsql`/Turso — because each statically links its own SQLite C
core (a `compile_error!` in `main.rs` guards against enabling both). The on-disk
format is libSQL-compatible, and **the full black-box suite runs against both**:

```bash
# default (sqlite)
cargo test --all-targets

# libSQL backend — a separate binary build
cargo test --no-default-features --features libsql
```

The integration, security, and property suites are backend-agnostic *by
construction*: they drive the compiled binary and assert behavior (delivery,
read-tracking, routing, hardening), so whichever backend was compiled in is the
one under test. `doctor --json` simply reports `sqlite` or `libsql` and the test
accepts either. The same is true of the backend-agnostic unit modules
(`inject`, `model`, `config`, `setup`).

The one backend-specific layer is `src/store.rs`'s unit module, which is gated
`#[cfg(all(test, feature = "sqlite"))]` because it reaches into the concrete
`SqliteStore` (e.g. `s.conn.execute`). Under `--features libsql` those exact
store *semantics* are still covered, just through the black-box CLI/MCP roundtrips
rather than in-process calls. (Roadmap Phase 1 adds a generic
`assert_store_conformance(&dyn Store)` suite so both backends share one
in-process conformance harness.)

CI enforces the libSQL column as a real, blocking gate: a dedicated job runs
`cargo clippy --no-default-features --features libsql -- -D warnings` and
`cargo build --no-default-features --features libsql`. Phase 0 of the v0.2
roadmap closed the historical "both backends green" gap — both now implement the
full `Store` trait (reply/thread/receipts/touch_peer/`in_reply_to` + migration).

## 7. Benchmarks (`benches/weave_bench.rs`, criterion)

A `criterion` harness (`harness = false` in `Cargo.toml`) tracks the costs that
matter operationally. It mirrors the test harness (scrubbed env, unique temp DB)
but is standalone so the bench crate has no dependency on the test module tree:

- **`cold_start`** — `weave --version` (pure spawn + clap parse, store never
  opened) vs `weave doctor --json` (spawn + store open + peer query); the delta
  is the store-open cost a hook/agent pays.
- **`send_inbox`** — a full two-process `send` then `inbox --json` roundtrip
  against a fresh per-iteration DB (so inbox size stays constant).
- **`inbox_json_parse`** — a pure in-process `serde_json` parse of a synthetic
  `inbox --json` payload at 100 / 1,000 / 10,000 messages, reported as MiB/s,
  isolating deserialization throughput with no subprocess in the hot loop.

Run with:

```bash
cargo bench
```

## 8. Checklist: adding tests with a new feature

When you add or change a feature, add the matching coverage before you call it
done:

1. **Pure logic → a unit test** in the owning module's `#[cfg(test)]` block.
   Prefer making the core a pure function (like `commands_for`) so it can be
   asserted argv-for-argv with no subprocess.
2. **New CLI subcommand / flag → an integration test** in `tests/integration.rs`
   using the `tests/common` helpers (`run_ok`, `run`, `run_hook`,
   `run_stdin_full`). If it has machine-readable output, assert the `--json`
   shape, not just substrings.
3. **New MCP tool / protocol behavior → an `McpServer` test** (`spawn`,
   `call_tool`, assert `isError` and the returned text). Include the failure path
   (bad/oversized args → `isError`, never a panic or silent persist).
4. **New injector backend or shaping rule → unit tests** in `src/inject.rs`
   asserting the exact argv table, the end-of-options `--` guard for
   leading-dash bodies, the empty/whitespace no-op, and `id_valid` rejection of
   malicious target ids. If a real spawn matters, extend the fake-mux harness.
5. **A new invariant ("for any input, X holds") → a proptest property** in
   `tests/prop.rs`. Keep `cases` small (subprocess-heavy) and
   `failure_persistence: None`.
6. **A security or resource property → a `tests/security.rs` test**: verbatim
   delivery of hostile input, a confirm-gated destructive op, a length/identity
   cap, or a file-permission assertion.
7. **A new config field → extend the `config.rs` template tests** so the field is
   documented and the scaffold still parses.
8. **Touched the store API → keep both backends green.** Run the suite under
   `--no-default-features --features libsql` as well as the default, and make
   sure clippy passes for the libSQL build (CI gates both). If you add a column,
   add the additive migration and a roundtrip test.
9. **Performance-sensitive path → consider a criterion bench** so regressions are
   visible.
10. **Before pushing**, the full gate is: `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, and
    the libSQL clippy/build/test column.
