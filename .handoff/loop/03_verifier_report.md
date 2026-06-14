# 03 — Verifier report — WL-038..WL-042 combined batch

**Worktree:** `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch`, base `origin/develop`)
**Scope:** five stacked cards verified TOGETHER (WL-038 TTL, WL-039 idle-dedup, WL-040 session export/import, WL-041 read-back verify, WL-042 multi-provider).
**Delivery:** NOT committed/pushed/PR'd — leader owns delivery.

## OVERALL STATUS: GREEN

All six CI-gated combinations pass; the extra `surfaces` clippy passes; all four cross-boundary consistency checks pass; HOME-isolation confirmed; no test added (no missing layer found).

## Per-combination gate results

| # | Combination | Result | Counts |
|---|-------------|--------|--------|
| 1 | `cargo fmt --all --check` | **PASS** | clean |
| 2 | `cargo clippy --all-targets -- -D warnings` (sqlite) | **PASS** | no issues |
| 3 | `cargo clippy --no-default-features --features libsql -- -D warnings` | **PASS** | no issues |
| 4 | `cargo test --all-targets` (sqlite) | **PASS** | **706 passed, 0 failed, 0 ignored** |
| 5 | `cargo test --no-default-features --features libsql` | **PASS** | **657 passed, 0 failed, 1 ignored** |
| 6a | `cargo clippy --features sign -- -D warnings` | **PASS** | no issues |
| 6b | `cargo test --no-default-features --features "libsql sign"` | **PASS** | **697 passed, 0 failed, 1 ignored** |
| + | `cargo clippy --features surfaces -- -D warnings` | **PASS** | no issues |

**Note on the 1 ignored test (combos 5 & 6b):** it is the pre-existing env-gated live-remote Turso pull test
(`weave/tests/integration.rs:5942`, `#[ignore = "live remote Turso test; set WEAVE_TEST_TURSO_URL/_TOKEN…"]`).
It is NOT one of the WL-038..042 tests and is intentionally skipped (CI never sets the Turso env). No new
test is `#[ignore]`'d. sqlite (`--all-targets`) shows 0 ignored.

### Per-binary breakdown
- **sqlite (706):** 44 + 195 + 6 + 79 + 301 (integration) + 57 + 24.
- **libsql (657):** 43 + 195(1 ign) + 6 + 79 + 253 + 57 + 24.
- **libsql+sign (697):** 43 + 211(1 ign) + 6 + 92 + 264 + 57 + 24.

## Cross-boundary consistency checks (the verifier's job)

### 1. Two new messages columns — IDENTICAL positional projection order in both backends — PASS
- **libsql `row_to_message`** (`weave-core/src/store_libsql.rs:332`) reads positionally:
  `…priority=9, superseded_by=10, expires_at=11, kind=12`.
- **All 6 libsql message projections** (`store_libsql.rs:1530, 1593, 1612, 1637, 1662, 4552`) end with the
  identical tail `… in_reply_to, idempotency_key, trace_id, priority, superseded_by, expires_at, kind`
  — verified byte-identical across all six.
- **sqlite `row_to_message`** (`store.rs:1622`) reads BY NAME (`r.get("expires_at")`, `r.get("kind")`),
  so order-agnostic — fine.
- **sqlite thread CTE** reads POSITIONALLY and its projection
  `m.id … m.priority, m.superseded_by, m.expires_at, m.kind` (~line 3708) maps exactly to the closure
  reads `expires_at: r.get(11)`, `kind: r.get(12)` (`store.rs:3729/3731`) — matches the libsql positional
  order. **Verdict: projections agree across both backends at index 11 (expires_at) and 12 (kind).**

### 2. standing_mcp_surface_is_within_token_budget — PASS
WL-038 `ttl` and WL-039 `dedupIdle` were catalog-only (added to `tool_catalog()` op schemas, no new
standing tool). `mcp::tests::standing_mcp_surface_is_within_token_budget` runs and passes
(budget `MAX_STANDING_TOOLS_BYTES = 8192`). WL-040/041/042 added no MCP surface at all (CLI-only).
The `catalog_weave_send_lists_ttl` and `catalog_weave_notify_lists_dedup_idle` tests confirm the catalog
exposure path. **No standing-token growth.**

### 3. BROADCAST_SQL drift-guard — PASS
`model::tests::broadcast_sql_matches_broadcast` runs and passes
(`BROADCAST_SQL == broadcast_sql()`, byte-identical). The new columns did not touch the broadcast aliases.

### 4. HOME-isolation (WL-041/042 #1 risk) — PASS
- `scrub_env` (`weave/tests/common/mod.rs`) scrubs `XDG_CONFIG_HOME` but deliberately does NOT scrub `HOME`;
  `run_env` applies `extra_env` AFTER scrub so the test-supplied `HOME` wins.
- Every new WL-041/042 settings/provider test pins a UNIQUE temp HOME via
  `run_env(&db, &[…], &[("HOME", &home_str)])`, and `unique_tmp_dir` lands under `std::env::temp_dir()`
  (pid+nanos), never under real `$HOME`.
- Post-run audit of the developer's real config: `~/.claude/settings.json` has NO `/tmp` test-path leak
  (its weave refs are the pre-existing real install, mtime predates the run); `~/.codex/config.toml`
  (mtime 2026-06-07) and `~/.gemini/settings.json` (mtime 2026-06-11) unchanged; `~/.aider.conf.yml`
  absent (no test wrote it to real HOME). **No clobber of the developer's real environment.**

## Test-layer completeness (no test added)

Every card ships its matching test layer; all key new tests are registered in the compiled binaries and
ran inside the green counts above (verified via `--list`):
- **WL-038:** model unit, sqlite-store unit (`expiry_stamps_and_excludes_from_unread`,
  `sweep_expired_messages_deletes_expired_keeps_live`, …), libsql-store twins, MCP catalog,
  integration, security (`ttl_is_capped`, `expired_ephemeral_is_not_recoverable`), prop.
- **WL-039:** sqlite/libsql store units (`supersede_prior_idle_replaces_prior_unread_idle`,
  `idle_dedup_never_touches_real_messages` + `_libsql` twin), MCP catalog, integration.
- **WL-040:** pure `session.rs` units, cross-DB integration
  (`session_export_import_round_trips_across_distinct_dbs`), 8 security tests.
- **WL-041:** setup.rs predicate units, 5 integration (temp-HOME), 1 security.
- **WL-042:** setup.rs provider units, 5 integration (temp-HOME, incl. claude-byte-identity regression
  + invalid-provider clap rejection).

No missing test layer was found, so **no test was added by the verifier**.

## Routing

Nothing RED. No items to route back to weave-implementer. Tree is verified and ready for the guardian's
invariant/drift/docs review.
