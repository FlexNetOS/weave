# WL-026 Verifier Report

## Tests added
| Layer | File | Cases |
|---|---|---|
| Unit (model) | `weave-core/src/model.rs` | `idempotency_key_valid_bounds`, `trace_id_valid_bounds` |
| Unit (store) | `weave-core/src/store.rs` | `send_idempotency_returns_existing_id`, `send_trace_id_roundtrips`, `send_idempotency_key_and_trace_id_on_outbox` |
| Integration | `weave/tests/integration.rs` | `cli_send_idempotency_dedupes`, `cli_send_trace_id_in_json` |
| Security | `weave/tests/security.rs` | `idempotency_key_oversized_is_rejected`, `idempotency_key_hostile_is_rejected` |

## Full gate results

### sqlite (default)
- `cargo fmt --all --check`: exit 0
- `cargo clippy --all-targets -- -D warnings`: exit 0
- `cargo test --all-targets`: 507 passed, 0 failed, 0 ignored

### libsql
- `cargo clippy --no-default-features --features libsql -- -D warnings`: exit 0
- `cargo build --no-default-features --features libsql`: exit 0
- `cargo test --no-default-features --features libsql`: 467 passed, 0 failed, 1 ignored

## Cross-boundary checks
- **Store trait ↔ both impls:** `send` and `enqueue_intent` signatures identical; idempotency guard semantics identical (SELECT-then-INSERT).
- **Schema ↔ migrations:** Fresh DBs get inline `UNIQUE` on `messages.idempotency_key`; legacy DBs get additive column + separate `CREATE UNIQUE INDEX` (SQLite `ALTER TABLE ADD COLUMN` rejects inline UNIQUE on non-empty tables).
- **Row mappers:** `row_to_message` and `row_to_intent` updated in both backends; all explicit SELECT projections include the new columns.
- **MCP schema ↔ handler:** `idempotencyKey` added to both `weave_send` and `weave_notify` input schemas; handlers read and pass through.

## Overall status
**GREEN**
