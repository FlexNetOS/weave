# Verifier report — WL-035 + WL-037 + WL-036 combined batch

- **Worktree:** `/home/drdave/Desktop/meta/weave-batch`
- **Branch:** `wl-035-037-batch` (3 commits on `develop`: 08608d6 WL-035, eff25ea WL-037, f4142c5 WL-036)
- **Verdict: GREEN** — full dual-backend gate passes; every required test layer added; cross-boundary checks all agree.
- No commit/push/gh performed (leader delivers). Only test code + `cargo fmt --all` touched files (no production-logic edits were needed).

## The `JobState` flag — NOT a real warning on this branch

The implementer flagged a possibly-pre-existing `unused import: JobState` in `weave-core/src/store.rs`.
**Investigated and dismissed:** `JobState` is genuinely used (`store.rs:1656`, `4319`, `4412`, `4454`, `4543`, …). All clippy variants below (default, libsql, surfaces, sign, libsql+sign) are clean under `-D warnings`. No fix needed; nothing to remove.

## Tests added (file + case names)

### WL-035 — backup/restore
- `weave-core/src/archive.rs` — already had **9** unit tests (round-trip, empty/512-aligned body, absent member, truncation reject, checksum reject, traversal-guard accept/reject, read-back guard). Verified comprehensive; **no gap to extend**.
- `weave-core/src/store.rs` (sqlite unit): `snapshot_to_roundtrips_messages`, `snapshot_to_empty_db_is_valid`.
- `weave-core/src/store_libsql.rs` (libsql unit): `snapshot_to_roundtrips_messages_libsql` (local `VACUUM INTO` + read-back).
- `weave/tests/integration.rs`: `backup_then_restore_into_fresh_db_preserves_messages` (send 2 → backup → restore into a fresh `WEAVE_DB` → messages survive, restore note + "run `weave setup`" printed), `backup_refuses_to_overwrite_without_force` (overwrite refused sans `--force`, succeeds with it).
- `weave/tests/security.rs`: `restore_refuses_path_traversal_entry` (crafted tar with a `../escape` entry built via the pure `weave_core::archive::write_archive`; `weave restore` REFUSES and **nothing is written outside the target dir** — asserts the escape path does not exist).

### WL-037 — message supersede / successor chains
- `weave-core/src/store.rs` (sqlite unit): `supersede_stamps_predecessor`, `superseded_message_hidden_from_unread`, `history_retains_superseded_with_flag`, `supersede_chain_only_tail_unread`, `supersede_rejects_foreign_sender`, `supersede_rejects_missing_ids`, `supersede_broadcast_drops_from_all_readers`, `supersede_migration_is_idempotent`.
- `weave-core/src/store_libsql.rs` (libsql unit — the **positional-projection (index 10)** risk): `supersede_stamps_and_hides_from_unread_libsql`, `supersede_chain_and_history_flag_libsql`, `supersede_rejects_foreign_and_missing_libsql`, `supersede_broadcast_drops_from_all_readers_libsql`.
- `weave/tests/integration.rs`: `supersede_hides_predecessor_from_unread_keeps_in_audit` (unread inbox shows only successor; `search` audit surface keeps the predecessor flagged with its successor id), `supersede_cross_identity_is_rejected`, `supersede_missing_id_errors_cleanly` (missing id → clean error; negative id → "positive" rejection).
- `weave-mcp` `McpServer` test (`weave/tests/integration.rs`): `mcp_weave_send_supersedes_post_stamps_and_failure_path` — `weave_send {supersedes}` post-stamps and the predecessor is hidden from a later `weave_inbox`; **failure path** = cross-identity supersede and `supersedes:0` both return `isError` (never a panic, never a silent persist). Runs under BOTH backends (the integration suite is built per-backend).
- `weave/tests/security.rs`: `supersede_cannot_censor_another_agents_message` (censorship/DoS guard: cross-identity supersede rejected, the targeted message stays unread).

### WL-036 — post-send hooks
- `weave-core/src/config.rs` (unit): `hook_recipient_matches_wildcard_exact_broadcast_case` (`*`/exact/BROADCAST-alias/non-match/case-sensitivity), `hook_event_parse_is_total`, `post_send_hook_toml_parses`, `post_send_hook_timeout_is_clamped`, `post_send_hook_is_valid_caps`, `hooks_for_selects_by_event_recipient_and_validity`.
- `weave/tests/integration.rs`: `post_send_hook_fires_with_env_and_skips_non_match` — a `[[post_send_hook]]` whose argv is a trusted helper (placed under a `WEAVE_MUX_DIR` dir) writes a sentinel from the `WEAVE_HOOK_*` env; the matching recipient fires with the correct env-derived content, a **non-matching** recipient does NOT fire, and the message **BODY is NOT in the payload** (`secret-body-content` never reaches the child).
- `weave/tests/security.rs`: `post_send_hook_hostile_subject_is_inert` (subject `"; touch CANARY ; $(reboot) \`id\`"` reaches the child as a **verbatim inert env value**; the injected `touch` canary never appears → no shell ran), `post_send_hook_untrusted_program_refused_send_still_succeeds` (an untrusted `argv[0]` is refused — no spawn — and the send still persists; the bounded-synchronous fault-isolated spawn this exercises is the same path that makes a slow/failing hook non-fatal to send).

**Test-code issues I fixed myself** (no production bug): `--supersedes -1` had to become `--supersedes=-1` (clap parses a bare `-1` as a flag); `weave search` takes `--query` not a positional. Both were my own test-authoring slips, corrected and re-run green.

## Full gate — both backends (each command + exit code)

| Command | Backend | Result | Exit |
|---|---|---|---|
| `cargo fmt --all --check` | — | clean (after `cargo fmt --all` applied to new test code) | 0 |
| `cargo clippy --all-targets -- -D warnings` | sqlite (default) | No issues | 0 |
| `cargo test --all-targets` | sqlite (default) | all pass | 0 |
| `cargo clippy --no-default-features --features libsql -- -D warnings` | libsql | No issues | 0 |
| `cargo test --no-default-features --features libsql` | libsql | all pass | 0 |
| `cargo clippy --features surfaces -- -D warnings` | surfaces | No issues | 0 |
| `cargo clippy --features sign -- -D warnings` | sign | No issues | 0 |
| `cargo test --features sign` | sign | all pass | 0 |
| `cargo clippy --no-default-features --features "libsql sign" -- -D warnings` | libsql+sign | No issues | 0 |

### Per-suite test counts

**Default (sqlite) — `cargo test --all-targets`:**
- `weave` (bin unittests): 26
- `tests/integration.rs`: 175
- `tests/prop.rs`: 4
- `tests/security.rs`: 68
- `weave_core` lib unittests: 276
- `weave_inject` lib unittests: 57
- `weave_mcp` lib unittests: 20
- **Total: 626 passed, 0 failed, 0 ignored** (was 599 pre-batch → **+27** new tests).

**libsql — `cargo test --no-default-features --features libsql`:**
- `weave` bin: 25
- integration: 175 (1 ignored = the pre-existing env-gated live-Turso pull test at `integration.rs:5757`, NOT mine)
- prop: 4
- security: 68
- `weave_core` lib: 232
- `weave_inject` lib: 57
- `weave_mcp` lib: 20
- **Total: 581 passed, 0 failed, 1 ignored** (the ignored one is a live-remote test, correctly skipped; no `#[ignore]` was added by this pass).

**sign — `cargo test --features sign`:** 26 / 191 / 4 / 81 / 294 / 57 / 20 — all pass, 0 failed, 0 ignored.

## Cross-boundary checks performed

| Check | Verdict |
|---|---|
| `Store` trait ↔ both impls for `supersede(&self, caller, old_id, new_id) -> Result<()>` | **AGREE** — byte-identical signature in trait (`store.rs:728`), sqlite (`store.rs:4965`), libsql (`store_libsql.rs:4170`); same authz + existence + UPDATE semantics. |
| `Store` trait ↔ both impls for `snapshot_to(&self, dest: &Path) -> Result<()>` | **AGREE** — trait `store.rs:774`, sqlite `store.rs:3150`, libsql `store_libsql.rs:1693`; both use parameterized `VACUUM INTO ?1` + read-back verify; libsql remote `bail!`s. |
| libsql positional `row_to_message` (index 10) ↔ every extended projection | **AGREE** — proven by 4 libsql supersede unit tests + the full libsql integration run: stamps/hide/chain/history-flag/broadcast all read back correctly (the highest-risk WL-037 item). |
| MCP `weave_send` inputSchema (`supersedes` integer, `mcp.rs:3236`) ↔ handler (`mcp.rs:597` reads + rejects `<=0`, `mcp.rs:676` post-stamps) | **AGREE** — schema property matches what the handler reads/validates; the catalog op rides existing `weave_send`, no new standing tool. |
| `BROADCAST` ↔ `BROADCAST_SQL` drift guard (WL-036 matcher reuses `model::is_broadcast`) | **HOLDS** — `model::broadcast_sql_matches_broadcast` + `every_broadcast_alias_is_in_sql` pass; the hook matcher uses the single-source alias set, no new alias list. |
| token-light standing MCP budget (WL-037 schema + WL-036 hooks add zero standing tokens) | **HOLDS** — `standing_mcp_surface_is_within_token_budget` + `progressive_default_surface_is_just_the_meta_tool` pass. |
| WL-035 archive writer entry-set ↔ extractor accept-list (`safe_entry_name` closed set) | **AGREE** — traversal guard is a closed accept-list of flat constants; the `../escape` security test confirms a non-member/`..` name is rejected and nothing escapes. |

## No real behavior bugs found

The bug classes this pass exists to catch — a libsql positional mismatch, a hook firing on a non-match, a body leaking into the hook child, a supersede authz hole, a tar traversal escape — were all exercised and **all behave correctly**. No production-code bug was surfaced; no fix was routed back to the implementer.

**Overall status: GREEN.**
