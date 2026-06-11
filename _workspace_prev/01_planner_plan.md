# WL-027 Plan: Broadcast notify / broadcast ask

## Goal
Add fan-out notifications and asks to all online peers in the caller's circle, not just the existing `--to all` store broadcast alias. This is repowire parity.

## Current state
- `send --to all` writes ONE message row with `recipient = "all"`; read-time `IN` clause makes it visible to everyone.
- `notify` and `ask` explicitly REJECT broadcast aliases.
- `ask_many` is the only existing fan-out: caller enumerates peers explicitly, store creates one child per peer, caller nudges each.
- Circle is a peer-listing filter, not a message-layer scope.
- Presence/liveness: `is_alive()` checks TTL (15 min) + PID for local peers.

## Touched files
| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/store.rs` | store | Add `broadcast_notify(me, subject, body)` and `broadcast_ask(me, subject, body, reply_to)` default methods on `Store` trait that enumerate online peers in circle and fan out | Core fan-out logic |
| `weave-core/src/store.rs` | store | Add `list_online_peers_in_circle(circle)` helper | Filter peers by liveness + circle |
| `weave/src/main.rs` | main | Add `BroadcastNotify { subject, body }` and `BroadcastAsk { subject, body, reply_to }` CLI subcommands | User-facing |
| `weave-mcp/src/mcp.rs` | mcp | Add `weave_broadcast_notify` and `weave_broadcast_ask` MCP tools | MCP-facing |
| `weave/tests/integration.rs` | tests | Add CLI roundtrip tests for broadcast notify/ask | Integration |
| `weave/tests/security.rs` | tests | Add caps tests (max peer count, body length) | Security |

## Dual-backend?
**Yes.** The `Store` trait changes need to compile on both backends. The fan-out logic should be default trait methods that call existing backend-specific methods (`list_peers`, `send`, `ask`), so minimal per-backend code.

## Invariants in scope
- Input caps: body length, subject length
- No shell: no new external process spawning
- MCP stdout discipline

## Test layers required
| Layer | Cases |
|---|---|
| Unit (store) | `broadcast_notify` creates one row per online peer; offline peers skipped |
| Integration | CLI `broadcast-notify` and `broadcast-ask` roundtrip |
| Security | Oversized body rejected; max peer count enforced |

## Edit order
1. `store.rs`: Add `list_online_peers_in_circle` and `broadcast_notify`/`broadcast_ask` default trait methods.
2. `store_libsql.rs`: Ensure `list_peers` returns enough data for liveness filtering.
3. `main.rs`: Add CLI subcommands.
4. `mcp.rs`: Add MCP tools.
5. Tests.
6. Full gate both backends.

## Risks / open questions
- Should offline peers get a queued message anyway (like store broadcast does), or should broadcast be strictly online-only? **Tentative: online-only for notify, online-only for ask** — this matches the "push to live panes" intent of broadcast.
- How to report per-peer results? Aggregated list like `ask_many` outcome.
- Circle scope: use the caller's own circle from config, or accept `--circle` override? **Tentative: accept optional `--circle`; default to caller's configured circle.**
