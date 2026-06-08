# WL-026 Implementer Changes

## Files touched
| File | Change |
|---|---|
| `weave-core/src/model.rs` | Added `idempotency_key` and `trace_id` to `Message` and `Intent`; added `MAX_IDEMPOTENCY_KEY_LEN`, `MAX_TRACE_ID_LEN`, `idempotency_key_valid`, `trace_id_valid`, `mint_trace_id()` |
| `weave-core/src/store.rs` | Updated `Store` trait `send` + `enqueue_intent` signatures; updated schema + migration; added idempotency guard to `SqliteStore::send`; validation for key/id caps; updated `row_to_message`, `row_to_intent`, thread query, outbox SELECTs |
| `weave-core/src/store_libsql.rs` | Mirrored all `store.rs` changes for `LibsqlStore`; updated schema, migrations, `send`, `enqueue_intent`, row mappers, SELECT projections |
| `weave/src/main.rs` | Added `--idempotency-key` to `Send` and `Notify`; auto-mint `trace_id` via `model::mint_trace_id()`; pass both through to store |
| `weave-mcp/src/mcp.rs` | Updated `weave_send`/`weave_notify` tool schemas with `idempotencyKey`; auto-mint trace_id; pass through to store; updated `tool_tick` send call |
| `weave/tests/integration.rs` | Added `cli_send_idempotency_dedupes` and `cli_send_trace_id_in_json` |
| `weave/tests/security.rs` | Added `idempotency_key_oversized_is_rejected` and `idempotency_key_hostile_is_rejected` |
| `weave-core/src/store.rs` | Added unit tests: `send_idempotency_returns_existing_id`, `send_trace_id_roundtrips`, `send_idempotency_key_and_trace_id_on_outbox` |
| `weave-core/src/model.rs` | Added unit tests: `idempotency_key_valid_bounds`, `trace_id_valid_bounds` |

## Dual-backend
**Yes.** Every Store trait change, schema change, migration, and row mapper was mirrored in both `store.rs` and `store_libsql.rs`.

## Build results
- `cargo build` (sqlite): ok
- `cargo build --no-default-features --features libsql`: ok
- `cargo fmt --all --check`: ok
- `cargo clippy --all-targets -- -D warnings`: ok
- `cargo clippy --no-default-features --features libsql -- -D warnings`: ok
