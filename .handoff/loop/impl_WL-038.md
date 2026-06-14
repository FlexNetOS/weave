# WL-038 — Ephemeral messages with TTL + auto-sweep — Implementation change log

Status: implemented; both backends compile clean; sqlite suite green (644 passed).
Branch/worktree: `wl-038-042-batch` @ `/home/drdave/Desktop/meta/weave-wl038-042`.
Delivery: NOT committed/pushed (leader owns delivery).

## Build / test verification

- `cargo build` (default sqlite) — clean
- `cargo build --no-default-features --features libsql` — clean
- `cargo clippy --all-targets -- -D warnings` (sqlite) — clean
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` — clean
- `cargo fmt --all --check` — clean
- `cargo test --all-targets` (sqlite) — **644 passed** (was 628 pre-change)
- `cargo test --no-default-features --features libsql` — **598 passed, 1 ignored**
- `cargo build --features sign` and `--no-default-features --features "libsql sign"` — clean

## Store / backend boundary: CROSSED (dual-backend)

Every `messages`/`outbox` SQL + Store-trait change was mirrored in both
`weave-core/src/store.rs` (default sqlite) and `weave-core/src/store_libsql.rs`
(feature-gated libsql). The libsql message projections are positional —
`expires_at` is the trailing index-11 column in every `SELECT ... FROM messages`
that feeds `row_to_message`, and `outbox.ttl` is index-11 in the intent projections.

## Files touched (rationale each)

### Code
- `weave-core/src/model.rs` — `Message.expires_at: Option<i64>` (+`#[serde(default)]`),
  `Intent.ttl: i64` (+`#[serde(default)]`), `MAX_MSG_TTL_SECS = 86_400`,
  `ttl_valid`, `expiry_from_ttl` (saturating). + unit tests.
- `weave-core/src/store.rs` — SCHEMA `messages.expires_at` (trailing) + `outbox.ttl`;
  two guarded `ADD COLUMN` migrations; `row_to_message` (by-name) + thread projection
  (index 11) + peek projection; read-surface expiry guards + `unread_count_conn` guard;
  `set_message_expiry` + `sweep_expired_messages` trait methods & impls; `gc()` expired-
  ephemeral fold-in; opportunistic sweeps before inbox/peek/history/search/inbox_since;
  `enqueue_intent` gains `ttl` param + `outbox.ttl` bind; `row_to_intent`/`list_outbox`/
  `outbox_all` project `ttl`; pull-commit (`commit_pulled`) re-stamps `set_message_expiry`
  when `intent.ttl > 0`. + store unit tests.
- `weave-core/src/store_libsql.rs` — item-for-item mirror of the above (positional
  projections, async/block_on). + store unit tests (libsql twins).
- `weave-core/src/export.rs` — added `expires_at: None` to the test-helper `Message`.
- `weave-mcp/src/mcp.rs` — `tool_send`/`tool_notify`/`tool_reply` read+cap-validate `ttl`,
  post-stamp `set_message_expiry`, pass `ttl` into cross-store `enqueue_intent`;
  `tool_catalog()` adds a `ttl` schema property to `weave_send`/`weave_notify`/`weave_reply`
  (**catalog only — NO new standing tool**; the standing-budget test stays green).
  + MCP tests.
- `weave-mcp/src/dashboard.rs` — added `expires_at: None` to the test-helper `Message`.
- `weave/src/main.rs` — `Cmd::Send` gains `--ttl Option<i64>`; CLI-seam cap validation;
  local post-stamp `set_message_expiry`; cross-store pass-through into `enqueue_intent`.

### Tests added
- `weave-core/src/model.rs`: `ttl_valid_bounds`, `expiry_from_ttl_adds_and_saturates` (2).
- `weave-core/src/store.rs` (sqlite store unit): `expiry_stamps_and_excludes_from_unread`,
  `sweep_expired_messages_deletes_expired_keeps_live`, `gc_also_reaps_expired_ephemeral`,
  `non_ephemeral_message_is_never_swept`, `expires_at_column_is_migrated_idempotently`,
  `cross_store_intent_carries_ttl_to_expiry` (6).
- `weave-core/src/store_libsql.rs` (libsql store unit): 5 twins
  (`*_libsql`: expiry-excludes, sweep, gc-reaps, never-swept, cross-store-ttl).
- `weave-mcp/src/mcp.rs`: `weave_send_ttl_stamps_expiry`, `weave_send_ttl_zero_is_rejected`,
  `catalog_weave_send_lists_ttl` (3).
- `weave/tests/integration.rs`: `send_ttl_message_round_trips_with_expiry`,
  `send_ttl_rejects_out_of_range`, `send_ttl_cross_store_carries_through` (3).
- `weave/tests/security.rs`: `ttl_is_capped`, `expired_ephemeral_is_not_recoverable` (2).
- `weave/tests/prop.rs`: `expiry_monotonicity`, `expiry_saturates_without_panic` (2).

**New test count: 23** (2 model + 6 sqlite-store + 5 libsql-store + 3 mcp + 3 integration
+ 2 security + 2 prop).

### Docs (shipped with the code)
- `CHANGELOG.md` — `[Unreleased] / Added` WL-038 bullet (WL-037 style).
- `docs/REPOWIRE-PARITY.md` — new "Ephemeral messages / TTL auto-sweep (atm-core)" → HAVE row.
- `ARCHITECTURE.md` — `expires_at` lifecycle (absolute deadline, delete-on-sweep, gc fold-in
  + opportunistic `sweep_expired_messages`, read-surface guard, cross-store via `outbox.ttl`,
  broadcast note).
- `README.md` — `weave send --ttl <secs>` usage line.
- `docs/TESTING.md` — Property 6 (expiry monotonicity) note + pointer to the dual-backend /
  security coverage.

## Deviations from the plan (with reasoning)

1. **CLI `--ttl` scoped to `weave send` only** (not also `Cmd::Notify`/`Cmd::Reply`). The
   plan suggested wiring `--ttl` into Notify/Reply "for parity if those carry priority
   today". `weave send` is the single user-facing ephemeral surface documented in the
   README; the MCP `weave_notify`/`weave_reply` DO carry `ttl` (catalog + handler), so MCP
   parity is complete. Keeping the CLI surface minimal avoids touching the broadcast-notify
   fan-out path and reduces blast radius. The store/MCP layers fully support ttl on those
   ops should a follow-up want the CLI flags. (No invariant impact.)

2. **MCP `tool_reply` did NOT previously post-stamp priority** (the plan asserted
   notify/reply "already post-stamp priority"). I added the ttl read + cap-validate +
   `set_message_expiry` post-stamp to `tool_reply` directly, mirroring the send/notify
   pattern — so reply now supports ephemeral too, consistent with the catalog property.

3. **`history` CLI subcommand does not exist** — the security non-recoverability test asserts
   the body is gone via `inbox --all --peek` (read-history surface) + `search` + `export`
   instead of a non-existent `weave history`. Same coverage (delete-on-sweep means every
   read surface + export loses the body).

4. **Deterministic expiry in the security test uses a 2s real sleep + `weave gc`** (matching
   the existing `lease_list_only_active` 2s-sleep precedent) because no CLI path can stamp a
   *past* expiry. The precise-expiry assertions (`set_message_expiry(id, now()-1)`) live in
   the dual-backend store unit tests, which are wall-clock-free; the CLI/integration tests
   assert only the round-trip (a positive `expires_at`) + cap rejection + cross-store carry.

## Invariants upheld

- Parameterized SQL throughout (every `expires_at`/`ttl`/now-cutoff bound via `params!` /
  `params(vec![...])`; only additive DDL identifiers are literals).
- No-shell: ttl is numeric, never reaches a spawn.
- Input caps: `MAX_MSG_TTL_SECS`/`ttl_valid` at both CLI and MCP seams; `saturating_add`.
- MCP stdout discipline: no new stdout writes.
- Token-light MCP surface (ADR-0003): ttl added ONLY to `tool_catalog()` schemas — no new
  standing tool; `standing_mcp_surface_is_within_token_budget` stays green.
- Additive/backward-compatible: nullable column, `#[serde(default)]`, NULL/0 == legacy;
  old DBs migrate in place (O(1) `ADD COLUMN`).
