# Plan — WL-040b: Faithful ask-thread replay on session import

## Goal

WL-040 (`weave session export/import`, merged #93) already serializes the `asks`
array into the canonical envelope, but the import path **skips** it (prints
"N asks in archive not imported — see WL-040b"). WL-040b makes import faithfully
**replay** each exported ask thread into the target DB in whatever terminal or
non-terminal `AskState` it was exported in (including already-`answered` and
already-`acked`), wiring `question_msg_id`/`answer_msg_id` to the **freshly
re-minted** local message ids (WL-040 mints fresh ids via `Store::send`), applying
the same `--as` identity remap to asker/askee, staying idempotent on re-import,
and reporting "N asks replayed" instead of "skipped". A new dual-backend
`Store::import_ask(...)` does the out-of-order materialization (the normal
create→answer→ack lifecycle cannot reach a closed state in one call); it is
mirrored in both `store.rs` (sqlite) and `store_libsql.rs` (libsql).

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/session.rs` | model (pure, no I/O) | Extend `ExportedAsk` with the missing durable fields `kind: String` (default `"free_text"`), `options: Option<String>`, `reply_to: Option<String>`, `close_note: Option<String>`, `parent_id: Option<String>` — all `#[serde(default)]`. Update export mapper note; the two existing round-trip unit tests gain the new fields. No schema-version bump needed (additive, `<= SCHEMA_VERSION` + unknown-field tolerance already covers older docs; the new fields default cleanly). Update the WL-040b "NOT imported" doc-comment on `ExportedAsk` + the `SessionExport.asks` field comment to "replayed via `Store::import_ask`". | The envelope currently carries only a subset of the `Ask` shape; faithful replay needs `kind`/`options`/`reply_to`/`close_note`/`parent_id` to reconstruct the row without lossy defaults. |
| `weave-core/src/store.rs` | store (sqlite, default) | (1) Add `import_ask(...)` to the `Store` **trait** (declaration near the other ask methods ~L417-455, `#[allow(dead_code)]` + `#[allow(clippy::too_many_arguments)]` like `ask`). (2) Implement it on `SqliteStore` (near `ask`/`answer`/`ack` ~L3950-4166). (3) Add a private `insert_imported_ask_row(...)` helper OR extend `insert_ask_row` — see "import_ask signature" below; recommend a **separate** insert path because `insert_ask_row` hardcodes `state=Open`, NULL answer/close/closed_ts. | The materialize-in-any-state insert + dedup live here for the default backend. |
| `weave-core/src/store_libsql.rs` | store (libsql, feature-gated) | Mirror `import_ask(...)` on `LibSqlStore` (near `ask` ~L2724, INSERT shape ~L2841). **Positional `params(vec![...])`** binding; column list MUST match the canonical asks projection order used by `row_to_ask`/`get_ask` (id, question_msg_id, answer_msg_id, asker, askee, subject, state, kind, options, reply_to, close_note, opened_ts, updated_ts, closed_ts, parent_id — 15 cols). | Dual-backend rule: both backends must compile + pass; libsql has no by-name binding so positional alignment is load-bearing. |
| `weave/src/session.rs` | bin (I/O orchestration) | (1) Add `validate_asks(&[ExportedAsk])` mirroring `validate_messages` (cap asker/askee via `check_ident`, subject ≤ `MAX_IMPORT_SUBJECT`, ask id via `ask_id_valid`, options/close_note length-bounded, parent_id via `ask_many_id_valid` when present, state parsable via `AskState::from_str`). Call it before any store write. (2) Build the `source_msg_id → new_local_id` remap **during** the message-insert loop (currently the loop only counts insert/skip; capture the `_id` returned by `store.send` keyed by `m.id`; for a skipped/deduped message, look up the existing local id — see "id-remap" risk). (3) After messages+memory, add the ask-replay loop: resolve remapped question/answer ids, apply `remap()` to asker/askee, call `store.import_ask(...)`, count replayed/skipped/dangling. (4) Replace `ask_note` ("not imported") with the replayed count in both dry-run and live summaries. | All file/store I/O + the cross-table id remap belong in the bin layer; the pure session module stays I/O-free. |
| `weave/tests/integration.rs` | test | New black-box export→import ask-replay test (see Test layers). | CLI/store behavior coverage. |
| `weave/tests/security.rs` | test | Dangling-reference + malformed-ask + input-cap cases. | Security/resource invariants. |
| `weave-core/src/store.rs` (+ `store_libsql.rs`) `#[cfg(test)]` | test | Unit: `import_ask` materializes a closed/answered ask correctly, both backends. | Pure store-method coverage incl. the failure path. |
| `docs/FORMAT-session-export.md` | doc | Respec the `asks` block from "recorded, not replayed" to "replayed faithfully via `import_ask`"; document the new `ExportedAsk` fields, the id-remap, dangling-skip policy, idempotency, and the ask_groups decision. | Doc must match new behavior. |
| `docs/REPOWIRE-PARITY.md` | doc | Update the casr "Session export / resume" row to note ask-thread fidelity is now complete (WL-040b). | Parity tracking. |
| `CHANGELOG.md` | doc | `[Unreleased]` entry: faithful ask-thread replay on session import (WL-040b). | User-facing change. |
| `ARCHITECTURE.md` | doc | If it enumerates the `Store` trait surface / session-import behavior, add `import_ask` + the "asks now replayed" note. (Verify §where the Store methods or session format are described; touch only if present.) | Keep architecture doc in sync. |
| `.handoff/loop/backlog.md` | doc | Mark WL-040b done with the merge evidence (already pre-checked `[x]` at L62 — confirm/keep, add commit ref on completion). | Backlog state. |

## Dual-backend? — YES

`import_ask` is a new `Store` trait method, so it crosses the dual-backend boundary
and MUST be mirrored:

- **`weave-core/src/store.rs`** — `SqliteStore::import_ask`: one `Transaction`
  (`Immediate`), parameterized `INSERT INTO asks (...) VALUES (?1..?15)` via
  named `params![]`, dedup-skip pre-check.
- **`weave-core/src/store_libsql.rs`** — `LibSqlStore::import_ask`: `self.rt.block_on`
  + `tx`/`conn.execute` with **positional** `params(vec![...])`. The 15-column
  INSERT order MUST equal the projection order in `row_to_ask` (positional indices
  0..14) — this is the libsql positional-alignment note: a column reorder silently
  corrupts the mapped row.

Build/lint/test BOTH backends (default sqlite + `--no-default-features --features
libsql`, and the `sign`/`libsql sign` combos CI gates).

### `import_ask` signature (proposed)

```
fn import_ask(
    &self,
    id: &str,                      // dedup key (see policy); regenerated, NOT the source id
    question_msg_id: i64,          // REMAPPED local id
    answer_msg_id: Option<i64>,    // REMAPPED local id, None if unanswered
    asker: &str,                   // already --as-remapped by caller
    askee: &str,                   // already --as-remapped by caller
    subject: Option<&str>,
    state: AskState,               // materialize directly in THIS state (open/answered/acked)
    kind: AskKind,
    options: Option<&str>,
    reply_to: Option<&str>,        // chain link (see ask_groups/reply_to note)
    close_note: Option<&str>,
    opened_ts: i64,
    updated_ts: i64,
    closed_ts: Option<i64>,
) -> Result<bool>                  // Ok(true)=inserted, Ok(false)=skipped (already present)
```

Notes: `import_ask` does **NOT** insert any `messages` rows and does **NOT** run the
`can_transition` lifecycle machine — it is a deliberate out-of-order materializer
(the question/answer message rows already exist from the WL-040 message-import
pass). It re-validates its own inputs at the store seam (defense-in-depth:
`check_ident` asker/askee, `ask_id_valid(id)`, body/subject not applicable here but
options/close_note length-capped, `state`/`closed_ts` consistency — recommend
asserting `state==Acked ⇒ closed_ts.is_some()` is *tolerated either way* rather than
hard-failing, since the source is authoritative). Returns whether a row was newly
inserted so the caller can count replayed-vs-skipped.

## Invariants in scope

- **No shell, ever** — `weave/src/session.rs` adds no external process spawn; pure store/file I/O. (constrains `weave/src/session.rs`)
- **Parameterize all SQL** — `import_ask` INSERT uses bound `params!`/`params(vec![...])` in BOTH backends; no string interpolation of asker/askee/options/state. (constrains `store.rs`, `store_libsql.rs`)
- **Input is capped / untrusted-input discipline** — every ask field bounded BEFORE the store write: `check_ident` on asker/askee, `ask_id_valid` on the ask id, `ask_many_id_valid` on parent_id, `MAX_IMPORT_SUBJECT` on subject, length caps on options/close_note, `AskState::from_str` rejecting unknown states. (constrains `weave/src/session.rs::validate_asks` + `import_ask` re-validation)
- **stdout discipline** — summary lines stay on stdout (CLI), no change to MCP frames. (constrains `weave/src/session.rs` print path only; no MCP surface added)
- **token-light MCP surface** — NO new standing MCP tool. `import_ask` is a `Store` method reached only by the CLI `session import` handler; the MCP surface is unchanged, so the standing-tools budget is untouched. (explicitly out of scope: do not add an `import_ask` MCP op)
- **Layer DAG** — `session.rs` (model) gains pure fields only; `store*.rs` (store layer) gains the method; `weave/src/session.rs` (bin) orchestrates. No upward dep. (constrains all three)

## Test layers required

1. **Unit — store method, BOTH backends** (`store.rs` + `store_libsql.rs` `#[cfg(test)]`):
   - `import_ask` materializes an **answered** ask: insert two messages, call `import_ask` with `state=Answered`, `answer_msg_id=Some(..)`; `get_ask` returns the row with `state==Answered`, correct `answer_msg_id`, `closed_ts==None`.
   - `import_ask` materializes an **acked/closed** ask: `state=Acked`, `closed_ts=Some(..)`, `close_note=Some(..)`; `get_ask` round-trips all of them.
   - `import_ask` is **idempotent**: calling twice with the same dedup id returns `Ok(true)` then `Ok(false)`, and `list_asks` count is unchanged.
   - (libsql copy of each, gated `#[cfg(feature="libsql")]` per the existing `list_asks_role_filtering` precedent at store_libsql.rs:4805.)
2. **Integration — black-box CLI** (`weave/tests/integration.rs`):
   - Build a source DB with a full ask thread: `ask` → `answer` → `ack` (one open, one answered, one acked for breadth). `weave session export --out f`. Import into a **fresh** `WEAVE_DB` via `weave session import --in f --as <id>`. Assert: stdout reports "N asks replayed"; `weave ask list` (or `list_asks` via a read CLI) shows the threads with the **remapped** message links resolving to the imported messages (question/answer bodies present, correct state).
   - **Re-import idempotent**: run import twice; second run reports 0 newly replayed (all skipped), `ask list` count stable.
   - `--dry-run` reports the would-replay count and writes nothing.
3. **Security/resource** (`weave/tests/security.rs`):
   - **Dangling reference**: craft an export whose ask `question_msg_id` is not in `messages[]`; import skips it with a counted warning (NOT a panic, NOT a partial/corrupt row). Decide+assert the policy (recommended: skip + count, since a dangling ask cannot be faithfully linked).
   - **Malformed ask record**: invalid ask id, oversized subject/options/close_note, unknown `state` string, hostile asker/askee → rejected by `validate_asks` BEFORE any store write (no row inserted).
   - **Input caps**: oversized options/close_note rejected; `parent_id` failing `ask_many_id_valid` rejected.
4. **Proptest** (`weave/tests/prop.rs`): only if a new pure invariant is introduced. None is strictly added (replay is materialization, not a new monotonic property), so **no new proptest required** — note this explicitly so the guardian does not flag a missing layer. (The existing `AskState` monotonic proptest still guards the normal lifecycle.)

## Docs to sync

- **`docs/FORMAT-session-export.md`** — rewrite the `asks` row in the field table and the "Asks — recorded, not replayed" section (L45, L76, L141-145) to: replayed faithfully via `import_ask`; document new `ExportedAsk` fields, id-remap of `question_msg_id`/`answer_msg_id`, dangling-skip policy, idempotency dedup key, and the ask_groups decision.
- **`docs/REPOWIRE-PARITY.md`** — update the casr "Session export / resume" row (L65) to mark ask-thread fidelity complete (WL-040b).
- **`CHANGELOG.md`** — `[Unreleased]`: "session: faithful ask-thread replay on import (WL-040b)".
- **`ARCHITECTURE.md`** — add `import_ask` to the Store surface / session-import description **iff** that surface is enumerated there (verify; touch only if present).
- **`.handoff/loop/backlog.md`** — confirm WL-040b `[x]` (L62) with completion/commit evidence.
- CONTRIBUTING/TESTING — no change expected (no new test *layer kind* introduced); `docs/TESTING.md` §8 checklist already covers store-method + integration + security.

## Edit order

1. `weave-core/src/session.rs` — extend `ExportedAsk` (new `#[serde(default)]` fields) + update doc-comments + the two round-trip unit tests. (No dependents break: additive.)
2. `weave-core/src/store.rs` — add `import_ask` to the `Store` trait declaration.
3. `weave-core/src/store.rs` — implement `SqliteStore::import_ask` (+ private insert helper) + store-side unit tests.
4. `weave-core/src/store_libsql.rs` — mirror `LibSqlStore::import_ask` (positional binding, column order pinned) + libsql-gated unit tests. (Trait now has a method both impls must satisfy — do 3 and 4 together so neither backend fails to compile.)
5. `weave/src/session.rs` — export mapper carries new fields; add `validate_asks`; build the `source→new` msg-id remap in the message loop; add the ask-replay loop; update both summary strings.
6. `weave/tests/integration.rs` — export→import→re-import ask-replay test.
7. `weave/tests/security.rs` — dangling/malformed/caps cases.
8. Docs: `FORMAT-session-export.md`, `REPOWIRE-PARITY.md`, `CHANGELOG.md`, `ARCHITECTURE.md` (if applicable), backlog.
9. Full gate, both backends: `cargo test --all-targets`; `cargo clippy --all-targets -- -D warnings`; `cargo build/clippy/test --no-default-features --features libsql`; `cargo fmt --all --check`.

## Risks / open questions

1. **Dedup key (OPEN — needs a decision).** `asks.id` is source-DB-scoped, so it cannot be reused verbatim (collisions across instances; the source PK is meaningless in the target). Two options:
   - **(A) Regenerate from the remapped question_msg_id** via `new_ask_id(new_question_msg_id)`. Deterministic *given* the remapped id, but a second import re-sends messages that DEDUP (same idempotency key → same existing local id), so `new_ask_id(same_id)` adds a random nonce → NOT idempotent. To make (A) idempotent, dedup the ask on `(asker, askee, question_msg_id)` (an ask already pointing at that question is the same thread) and skip-insert if present — recommended.
   - **(B) Synthesize a stable id** `wl040ask:<source_identity>:<source_ask_id>`-shaped (mirrors `synth_idempotency_key`), but it must satisfy `ask_id_valid` (≤64 chars, `[A-Za-z0-9_]`). The `:` separators FAIL `ask_id_valid`, so this needs `_` separators and a length budget — workable but tight against the 64-char cap with two long identities.
   - **Recommendation:** (A) + dedup on `(asker, askee, question_msg_id)` — robustly idempotent because the message remap is already idempotent, and no new id-shape to validate. Implementer to confirm; the `import_ask` `id` param then becomes the freshly-minted local id and the dedup pre-check is `SELECT 1 FROM asks WHERE asker=? AND askee=? AND question_msg_id=?`.
2. **id-remap for deduped messages (load-bearing).** The current message loop infers insert-vs-skip from a `total_messages()` delta and does NOT capture the local id of a *skipped* (already-present) message — but an ask may reference exactly such a message on re-import. The remap map must therefore resolve the local id for BOTH inserted and skipped messages. Cleanest: after `store.send(...)` (which is idempotent on idempotency_key and returns the existing id on a dedup hit — VERIFY `send` returns the existing rowid rather than -1/0 on dedup), key the map by `m.id → returned_id`. If `send` does NOT return the existing id on dedup, add a lookup-by-idempotency-key. Implementer must verify `Store::send`'s dedup return contract before relying on it.
3. **ask_groups / broadcast-ask replay (OPEN — propose DEFER).** The envelope does NOT carry `ask_groups` (parent anchor rows) at all today, and `parent_id` on child asks references a group that would not exist in the target. **Proposed scope for WL-040b: replay standalone asks + chained (`reply_to`) threads only; DEFER ask-many group (`parent_id`/`ask_groups`) reconstruction to a follow-up (WL-040c).** On import, if an exported ask carries a `parent_id`, replay it as a standalone ask with `parent_id=NULL` (faithful to its lifecycle/messages, lossy only on the group linkage) and count it, OR skip-with-warning. Recommend **replay as standalone, drop parent_id, with a counted note** so no ask thread is silently lost. Document this explicitly in FORMAT-session-export.md. Owner decision wanted: replay-orphaned vs defer-skip.
4. **`reply_to` chain integrity.** A replayed ask may carry `reply_to=<source ask id>`. Since ask ids are regenerated (risk 1), a stored `reply_to` pointing at the SOURCE id would dangle in the target. Options: (a) build a `source_ask_id → new_ask_id` map alongside the message map and rewrite `reply_to`; (b) NULL `reply_to` on import (lose the chain link but keep the thread). Recommend (a) if asks are replayed in export order (parents before children), else (b) with a note. This compounds with risk 1's id strategy — if dedup is on `(asker,askee,question_msg_id)` the new ask id is deterministic enough to build the map.
5. **Schema version.** Adding fields to `ExportedAsk` is additive and tolerated by the existing `<= SCHEMA_VERSION` + unknown-field rules, so **no `SCHEMA_VERSION` bump** is required; an older weave reading a new export simply ignores the new ask fields (and older weave already does not replay asks). Confirm this is acceptable vs. bumping to signal the richer ask block — recommend NOT bumping (consistent with the additive policy documented in `session.rs`).
