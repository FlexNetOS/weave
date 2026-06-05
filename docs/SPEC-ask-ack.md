# v0.2 spec — weave_ask / weave_ack (async task)

> Roadmap feature #2 (closes the biggest functional gap vs repowire/agent-teams:
> a synchronous-feeling ask/answer round-trip). SEP-1686-style async task pattern.

> **Status: P1 implemented (weave⊇repowire parity, epic 1).** The core of this spec
> shipped — see `CHANGELOG.md`, `ARCHITECTURE.md` §6 (`asks`), and the README "Tracked
> ask/answer/ack" section. What landed vs what is deferred:
>
> - **Shipped:** the `asks` side-table (both backends, guarded additive migration),
>   the question/answer text reusing `messages` (threaded via `in_reply_to`),
>   point-to-point-only (broadcast `to` rejected), and the `weave_ask` / `weave_ack` /
>   `weave_ask_get` / `weave_asks` MCP tools + CLI mirrors — exactly the storage and
>   composition decisions below.
> - **Shipped, but with a narrower lifecycle vocabulary than this spec proposed.** P1
>   ships the brief's canonical **`open → answered → acked`** monotonic lifecycle (plus
>   a dedicated **`weave_answer`** tool — the answer goes back along the correlation chain
>   to the asker — distinct from `weave_ack`, which is a pure close). The richer
>   **SEP-1686** state set below (`working | input_required | completed | failed |
>   cancelled`) is **deferred** to a later epic. The schema is **forward-compatible**:
>   `state` is stored as **TEXT** validated against a Rust `AskState` enum, so future
>   variants can be added **without a migration**.
> - **Shipped beyond this spec:** an opaque server-minted `correlation_id`
>   (`ask_<rowid>_<nonce>`, no `rand`/date crate), `reply_to` thread chaining (a new ask
>   acks the prior thread and links into its conversation), and an **honest delivery
>   verdict** (`transport_delivered` / `queued_next_turn` / `recipient_not_injectable`)
>   computed caller-side with **no `store → inject` edge** and **no new dependency**.
> - **Deferred:** the SEP-1686 richer states, broadcast/fan-out-join asks, cross-store
>   ask, and the stop-wake integration (Phasing §3 below). P1 is **local-mesh only**.
>
> The remainder of this document is the original forward-looking proposal, kept for the
> deferred work; read it against the Status note above for what is live today.

## Shape
A sender posts an **ask** (a request that needs a reply) to a peer and immediately
gets a **task id**. The peer answers later via **weave_ack**, moving the task
`working → completed` (or `input_required`/`failed`/`cancelled`). The sender polls
(`weave_ask_get`) or lists (`weave_asks`) for the result. It is **layered on the
existing mailbox**: an ask is also a normal `messages` row, so it nudges/drains/threads
like everything else, with a parallel `asks` row holding the mutable task state.

## Why a side table (not columns on `messages`)
`messages` is append-only and its read/receipt model is per-`(message, reader)`. An
ask is **mutable** (state + answer change after insert) and 1:1 with a request.
Overloading `messages` would make broadcast asks ambiguous and muddy the append-only
invariant the threading CTE + receipts rely on. So `asks` is its own table keyed by an
opaque task id — mirroring the existing `reads` side-table pattern (separate mutable
side-state, same DB, both backends).

Asks are **point-to-point only** in v1 (reject broadcast `to`); fan-out+join is out of scope.

## Data model (identical DDL in both backends)
```sql
CREATE TABLE IF NOT EXISTS asks (
    id          TEXT PRIMARY KEY,   -- "ask_<rowid>_<rand>", opaque
    message_id  INTEGER NOT NULL,   -- the messages row that carries the question
    asker       TEXT NOT NULL,
    askee       TEXT NOT NULL,
    state       TEXT NOT NULL,      -- working|input_required|completed|failed|cancelled
    answer      TEXT,               -- set by weave_ack
    created_ts  INTEGER NOT NULL,
    updated_ts  INTEGER NOT NULL
);
```

## Store trait additions (sync; both backends)
- `create_ask(asker, askee, body) -> Result<(String /*task id*/, i64 /*message id*/)>`
  (insert the question message + the `asks` row in one transaction).
- `ack_ask(id, answer, state) -> Result<()>` (state-machine guarded: only from a
  non-terminal state; sends the answer back as a reply message to the asker).
- `get_ask(id) -> Result<Option<Ask>>`; `list_asks(me, role) -> Result<Vec<Ask>>`.

## MCP tools + CLI
- `weave_ask {from, to, body}` → `{ task_id, state:"working" }`
- `weave_ack {id, answer, state?}` (default `completed`)
- `weave_ask_get {id}` → the Ask (state + answer)
- `weave_asks {me, role?}` → list
- CLI mirrors: `weave ask --to … --body …`, `weave ack --id … --answer …`,
  `weave ask-get --id …`, `weave asks [--me …]`.

## Composition
- The question and the answer are ordinary messages (threaded via `in_reply_to`), so
  the live nudge / hook drain / `weave thread` all work for free.
- A pending ask is the natural payload for the [stop-wake](SPEC-stop-wake.md): an
  `input_required`/unanswered ask is exactly what should drive the askee's next turn.

## Phasing
1. Schema + `Store` methods + conformance tests (both backends).
2. MCP tools + CLI.
3. Wire into the stop-wake so an open ask wakes the askee.
