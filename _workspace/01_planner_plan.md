# WL-026 Plan: Idempotency keys & trace IDs

## Goal
Add per-message `idempotency_key` and `trace_id` fields so callers can deduplicate retries and trace a single logical message end-to-end across stores and backends.

## Touched files
| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/model.rs` | model | Add `idempotency_key: Option<String>`, `trace_id: Option<String>` to `Message`; add `MAX_IDEMPOTENCY_KEY_LEN` (128), `MAX_TRACE_ID_LEN` (128); add `idempotency_key_valid` validator | Core data structure + caps |
| `weave-core/src/store.rs` | store | Update `Store::send` signature; add columns to `messages` schema; add additive migration; implement idempotency guard in `SqliteStore::send`; update `enqueue_intent`/`OutboxIntent` with `trace_id` | Sqlite backend |
| `weave-core/src/store_libsql.rs` | store | Mirror all `store.rs` changes for `LibsqlStore` | Libsql backend |
| `weave/src/main.rs` | main | Update `Cmd::Send`/`Cmd::Notify` to auto-mint `trace_id` (trace_<timestamp>_<6 random hex>) and accept optional `--idempotency-key`; surface both in JSON output | CLI |
| `weave-mcp/src/mcp.rs` | mcp | Update `weave_send`/`weave_notify` tool schemas with optional `idempotencyKey` and `traceId`; auto-mint trace_id when absent; pass through to store | MCP |
| `tests/integration.rs` | tests | Add `send_idempotency_roundtrip` and `trace_id_propagates_to_inbox` tests | Integration layer |
| `tests/security.rs` | tests | Add `idempotency_key_too_long_rejected`, `trace_id_too_long_rejected`, `hostile_idempotency_key_rejected` | Security layer |

## Dual-backend?
**Yes.** Every `Store` trait change and schema/migration change must be mirrored in both `store.rs` and `store_libsql.rs`.

## Invariants in scope
- **Parameterized SQL** — idempotency_key and trace_id bound as params, never interpolated.
- **Input caps** — keys capped at 128 chars; validated before store entry.
- **No shell** — not directly applicable, but CLI args pass through without shell involvement.
- **MCP stdout discipline** — tool responses stay pure JSON-RPC; trace IDs only in result bodies.

## Test layers required
| Layer | Location | Cases |
|---|---|---|
| Unit (model) | `weave-core/src/model.rs` | `idempotency_key_valid` accepts/rejects boundary values |
| Unit (store) | `weave-core/src/store.rs` | `send` with duplicate idempotency_key returns same `id`; `send` without key behaves as before |
| Integration | `tests/integration.rs` | CLI `--idempotency-key` dedupes; JSON output contains `trace_id`; inbox reflects both fields |
| Security | `tests/security.rs` | Oversized keys rejected; hostile chars stripped or rejected |
| Cross-boundary | n/a | Compare `store.rs` ↔ `store_libsql.rs` signatures and idempotency SQL |

## Docs to sync
- `CHANGELOG.md` [Unreleased] — note idempotency keys and trace IDs.

## Edit order
1. `model.rs` — fields, constants, validator.
2. `store.rs` — schema migration, trait signature, `SqliteStore::send` idempotency guard, `OutboxIntent` + `enqueue_intent`.
3. `store_libsql.rs` — mirror step 2.
4. `main.rs` — CLI args and trace-id minting.
5. `mcp.rs` — tool schema + handler updates.
6. Tests — integration + security.
7. Full gate on both backends.

## Risks / open questions
- **Idempotency scope:** globally unique key. A duplicate key anywhere in the circle returns the existing message id, regardless of sender. This matches distributed idempotency semantics (the key is the authority).
- **Trace ID format:** auto-minted as `trace_<ts>_<6 random hex>` when not provided by caller. Callers may supply their own for cross-system correlation.
- **Backward compatibility:** `serde(default)` on new `Message` fields ensures old JSON deserializes; additive DB migration ensures old databases open cleanly.
- **Outbox/intents:** `trace_id` is carried on `OutboxIntent` and written into the remote store's message row on pull. `idempotency_key` is also carried so cross-store delivery is idempotent.
