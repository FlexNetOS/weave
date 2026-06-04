---
name: weave-test-discipline
description: weave's multi-layer test strategy and the exact dual-backend verification gate (fmt + clippy -D warnings + test on BOTH sqlite and libsql). ALWAYS use when adding/changing weave behavior, writing tests, or deciding whether a change is done. Tells you which test layer a given change needs (unit / integration / security / proptest / bench) and the precise commands CI enforces. Do NOT use for the security invariants themselves (that is the weave-invariants skill).
---

# weave test discipline

weave is trusted with other agents' messages and with *typing into live terminals*. The test suite exists to protect that trust budget. The rule: **every change ships with the matching test layer, and passes the full gate on both storage backends.** Full detail in `docs/TESTING.md`.

## The full gate (what "done" means)

Run all of this before declaring a change complete. CI enforces every line.

```bash
# default (sqlite) backend
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets

# libsql backend — a SEPARATE, mutually-exclusive binary build
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build  --no-default-features --features libsql
cargo test   --no-default-features --features libsql
```

**Why both backends:** weave ships two mutually-exclusive storage backends — bundled `rusqlite` (default) and `libsql`/Turso — because each statically links its own SQLite C core (a `compile_error!` in `main.rs` guards against enabling both). The on-disk format is libSQL-compatible, and the black-box suites are backend-agnostic *by construction* (they drive the compiled binary), so the same behaviors must hold whichever backend is compiled in. **A store-touching change is not done until the libsql column is green too** — CI gates it as a blocking job.

**Run a single test:** `cargo test <name_substring>` (e.g. `cargo test tmux_is_paste_safe`); scope to one file with `cargo test --test integration <name>`.

## The five test layers

| Layer | Location | Style | Pins down |
|-------|----------|-------|-----------|
| Unit — pure logic | `src/*.rs` `#[cfg(test)]` | in-process | store semantics, injector argv shaping, model/broadcast invariants, config template, hook matcher |
| Integration — end-to-end | `tests/integration.rs` | black-box binary | MCP stdio protocol, CLI roundtrips, lifecycle hooks, `--json`/`doctor`/`gc`, backend selection |
| Security / hardening | `tests/security.rs` | black-box binary | flag-injection resistance, destructive-op guard, resource caps, at-rest `0600` file mode |
| Property-based | `tests/prop.rs` | black-box + proptest | routing correctness, read-tracking idempotence, unicode/long-body roundtrip |
| Benchmark | `benches/weave_bench.rs` | criterion | cold-start, send→inbox roundtrip, inbox-JSON parse throughput |

Black-box tests get a unique temp `WEAVE_DB` and a **scrubbed env** (every `WEAVE_*` and mux-detection var removed, `XDG_CONFIG_HOME` pointed at an empty dir), so they are isolated, parallel-safe, deterministic, and never touch the real store. Use the `tests/common` helpers (`run_ok`, `run`, `run_hook`, `run_stdin_full`, the `McpServer` driver).

## Which layer for which change

| Your change | Add a test in… |
|-------------|----------------|
| Pure logic in `model`/`inject`/`config` | the owning module's `#[cfg(test)]` block — assert exact values/argv, no subprocess |
| New CLI subcommand or flag | `tests/integration.rs`; if it has machine output, assert the `--json` **shape**, not substrings |
| New/changed MCP tool or protocol behavior | an `McpServer` test (`spawn`, `call_tool`); **include the failure path** — bad/oversized args → `isError`, never a panic or silent persist |
| New injector backend or shaping rule | `src/inject.rs` unit test: exact argv table, the end-of-options `--` guard for leading-dash bodies, the empty/whitespace no-op, `id_valid` rejection of malicious ids |
| A new "for any input, X holds" invariant | a proptest property in `tests/prop.rs` — keep `cases` small (subprocess-heavy), `failure_persistence: None` |
| A security/resource property | `tests/security.rs` — verbatim hostile-input delivery, confirm-gated destructive op, a length/identity cap, or a file-mode assertion |
| A new config field | extend the `config.rs` template tests so the field is documented and the scaffold still parses |
| Any `Store` method / column / migration | additive migration + a roundtrip test, **verified under both backends**; keep the trait and both impls in lockstep |
| Performance-sensitive path | consider a criterion bench so regressions are visible |

## Make the core pure so it can be unit-tested

Prefer a pure function (like `commands_for`) over one that both computes and performs I/O. Purity is why the entire injector shaping layer is asserted argv-for-argv with no terminal present. When you must test a real spawn, use the **fake-mux harness**: plant an executable `tmux` script on `PATH` that records its argv to a log, register a peer pretending to live in a pane, then assert the log shows the expected `send-keys -t <pane>` (see `tests/integration.rs`).

## Cross-boundary assertions (test the seam, not each side alone)

The highest-value tests compare two sides of an interface:
- `Store` trait ↔ `store.rs` **and** `store_libsql.rs` (same signature + semantics).
- `commands_for` output ↔ its exact-argv unit test.
- `BROADCAST` ↔ `BROADCAST_SQL` (the drift-guard test keeps them byte-identical).
- An MCP tool's advertised input schema ↔ what the handler actually validates (caps, `confirm`).

## Definition of done

1. The matching test layer(s) above are added.
2. The full gate passes on **both** backends.
3. No test is silently `#[ignore]`'d or a backend column skipped — any omission is called out.
4. New invariant → a proptest property locks it down for all inputs.
