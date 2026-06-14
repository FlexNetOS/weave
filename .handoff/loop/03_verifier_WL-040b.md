# Verifier report — WL-040b (faithful ask-thread + ask-group replay on session import)

**Worktree:** `/home/drdave/Desktop/meta/weave-wl040b` · **Branch:** `wl-040b-ask-replay` · **Base:** `origin/develop` @ `dcb36f1`
**Overall: GREEN.** All six CI-gated combinations pass. No commit/push/PR performed (leader owns delivery).

## Per-combination results

| # | Gate command | Result |
|---|---|---|
| 1 | `cargo fmt --all --check` | **PASS** (exit 0) |
| 2 | `cargo clippy --all-targets -- -D warnings` (default sqlite) | **PASS** (exit 0) |
| 3 | `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` | **PASS** (exit 0) |
| 4 | `cargo test --all-targets` (default sqlite) | **PASS** — 717 passed, 0 failed (7 suites) |
| 5 | `cargo test --no-default-features --features libsql` | **PASS** — 668 passed, 1 ignored, 0 failed (10 suites) |
| 6a | `cargo clippy --features sign --all-targets -- -D warnings` | **PASS** (exit 0) |
| 6b | `cargo test --no-default-features --features "libsql sign"` | **PASS** — 708 passed, 1 ignored, 0 failed (10 suites) |

The `1 ignored` under the libsql combos is a pre-existing remote-live test (`remote_live_pull_delivers_and_is_idempotent`, inert unless live env vars set) — not a WL-040b silencing.

## Pre-existing JobState warning — CONFIRMED non-blocker

- Reproduces on the branch under package-scoped `cargo clippy -p weave-core`: `unused import: JobState` at `weave-core/src/store.rs:11:63`.
- **Confirmed pre-existing on a clean `origin/develop` checkout** (throwaway `git worktree add --detach /tmp/weave-origin-check origin/develop` → same warning; worktree removed). Did not disturb the existing `stash@{0}`.
- WL-040b's diff to `store.rs:11` only **adds** `AskGroup`/`AskState` to the `use` list; it does **not** touch `JobState`. Its used/unused status is identical before and after.
- **CI-invisible:** CI runs `--all-targets` workspace clippy (combos 2/3/6a above, all exit 0). The warning fires only under non-`--all-targets` package-scoped clippy, which CI does not run.
- **Verdict: genuinely pre-existing + CI-invisible → NOT a blocker.** WL-040b introduced **zero** new clippy warnings.

## Cross-boundary checks

1. **`import_ask` 15-col INSERT (libsql, positional) ↔ `row_to_ask` indices 0..14 — ALIGNED.**
   Column order in both backends: `id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id`. Verified the libsql `import_ask` INSERT (`store_libsql.rs:3162-3185`), the canonical asks SELECT projection (`store_libsql.rs:3010-3012`), and `row_to_ask` (`store_libsql.rs:381-405`) all agree column-for-column; sqlite uses named `params![]` with an identical column list (`store.rs:4358-4380`). parent_id is index 14 in every projection.

2. **Re-import idempotency — HOLDS.** Both backends dedup on `(asker, askee, question_msg_id)` (sqlite `store.rs:4346`, libsql `store_libsql.rs:3141`), returning `Ok(false)` on hit. The `question_msg_id` is the remapped local id, and the message remap is itself idempotent (`Store::send` returns the existing id on idempotency-key hit), so a re-import lands on the same triple → skipped, not duplicated. The integration test `session_import_replays_ask_thread_and_group` asserts `0 ask(s) replayed` and a stable ask count on second import.

3. **Dangling ask skipped, not inserted-broken — HOLDS.** `weave/src/session.rs:362-376`: an ask whose `question_msg_id` is absent from the remap map → `ask_dangling += 1; continue` (never inserted); an ask claiming an `answer_msg_id` whose message is missing → also skipped. A `parent_id` whose group was not replayed is NULLed (standalone, not dangling). Dry-run counting (`session.rs:430-440`) mirrors the same exclusion. Security test `session_import_skips_dangling_ask_reference` covers it.

4. **`standing_mcp_surface_is_within_token_budget` — GREEN.** Test lives at `weave-mcp/src/mcp.rs:5658`, exercised by combo 4 (717 passed). WL-040b adds three `Store` methods + CLI/import wiring only — **no new standing MCP tool**, so the budget is unchanged.

5. **Temp WEAVE_DB isolation — HOLDS.** Integration/security tests use `TestDb::new()` (unique temp `WEAVE_DB`, scrubbed env per the `weave/tests/integration.rs:5` module contract). No test writes the developer's real `~/.claude` or default XDG store.

## Tests added (12)

- **Unit (sqlite, `weave-core/src/store.rs`):** `import_ask_materializes_answered_and_is_idempotent`, `import_ask_materializes_acked_and_group`, `import_ask_rejects_malformed_inputs`.
- **Unit (libsql, `weave-core/src/store_libsql.rs`):** `import_ask_materializes_answered_and_is_idempotent_libsql`, `import_ask_acked_and_group_libsql`, `import_ask_rejects_malformed_inputs_libsql`.
- **Pure model (`weave-core/src/session.rs`):** `older_export_without_new_ask_fields_defaults` (+ 2 existing round-trip tests extended with the new fields).
- **Integration (`weave/tests/integration.rs`):** `session_import_replays_ask_thread_and_group` (line 12153), `session_import_dry_run_counts_asks_without_writing` (line 12310).
- **Security (`weave/tests/security.rs`):** `session_import_skips_dangling_ask_reference`, `session_import_rejects_malformed_ask_state`, `session_import_rejects_oversized_ask_options`, `session_import_rejects_malformed_ask_parent_id`, `session_import_rejects_hostile_ask_asker` (lines 4379–4530).

## Routing

Nothing RED. No fix round-trip needed. Tree is verified and ready for guardian invariant/drift/docs review.
