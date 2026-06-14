# WL-037 — Message supersede / successor chains (atm-core parity)

## Goal
Let a sender replace a prior message with an updated one: the new message links to its predecessor, the predecessor is stamped as superseded, and readers see the latest in the chain by default (superseded messages are hidden from unread inbox, flagged in history/thread). This is **replacement**, distinct from the existing `in_reply_to` **threading** — the two columns coexist on the `messages` row and never interfere. The supersede link is an additive, backward-compatible column on `messages` mirrored across both storage backends, exposed via a `weave send --supersedes <id>` flag (post-stamp, mirroring WL-031 `--priority`) and a token-light MCP path (extend `weave_send` schema, no new standing tool).

## Touched files
| file | layer | what changes | why |
|---|---|---|---|
| `weave-core/src/model.rs` | model (no I/O) | add `superseded_by: Option<i64>` field to `Message` with `#[serde(default)]`; doc-comment it | the row shape carries the chain link |
| `weave-core/src/store.rs` | store (sqlite default) | SCHEMA `messages` gains `superseded_by INTEGER`; `migrate()` adds guarded `ALTER TABLE messages ADD COLUMN superseded_by INTEGER`; `row_to_message` reads it by name; `Store` trait gains `supersede(&self, caller, old_id, new_id) -> Result<()>`; read SQL in `inbox`/`inbox_since`/`peek_oldest_unread`/`unread_count(_conn)` filters out superseded; `history`/`thread`/`search` keep superseded but the mapper now populates the flag | default backend impl + read semantics |
| `weave-core/src/store_libsql.rs` | store (libsql, feature-gated) | mirror SCHEMA, mirror migrate table-driven `ADD COLUMN`, **extend every explicit `SELECT id,ts,...` projection to add `superseded_by` as the new trailing column AND update positional `row_to_message` (new index 10)**, mirror `supersede`, mirror the read-path filters | dual-backend parity; libsql reads by position not by name |
| `weave/src/main.rs` | main (bin) | `Cmd::Send` gains `--supersedes: Option<i64>`; after `store.send(...)` (+ existing priority stamp) call `store.supersede(&from, old, mid)` when set; print the link; optionally a dedicated `Cmd::Supersede` is NOT added (flag is enough) | CLI surface |
| `weave-mcp/src/mcp.rs` | mcp | `tool_send` reads optional `supersedes` arg and post-stamps via `store.supersede`; `tool_catalog()` `weave_send` inputSchema gains a `supersedes` property; **no new standing tool** | MCP surface, zero standing-token cost |
| `CHANGELOG.md` / `README.md` / `ARCHITECTURE.md` | docs | document supersede vs reply, the column, read semantics, schema note | drift guard |

## Dual-backend? YES
A new `messages` column + new `Store` method + changed read SQL → **both** backends must change and both build/test/clippy gates (`sqlite` default and `--no-default-features --features libsql`, plus `sign`/`libsql+sign`) must stay green.

Mirrored edits:
1. **SCHEMA** — `weave-core/src/store.rs` ~L1349-1360 (`CREATE TABLE messages`) and `weave-core/src/store_libsql.rs` ~L65-75. Add `superseded_by INTEGER` as the trailing column (after `priority`). Nullable, no default needed (NULL == not superseded).
2. **migrate()** — sqlite `weave-core/src/store.rs` `migrate()` (~L1845; precedent: the WL-031 priority `ADD COLUMN` at ~L2204-2214 and the `column_exists`-guarded `in_reply_to` add at ~L1849). libsql `weave-core/src/store_libsql.rs` table-driven loop (precedent: the WL-031 entry at ~L1031-1034 and WL-026 at ~L983-994). Add:
   - sqlite: `if !column_exists(conn,"messages","superseded_by")? { conn.execute_batch("ALTER TABLE messages ADD COLUMN superseded_by INTEGER;")?; }`
   - libsql: append `("messages","superseded_by","ALTER TABLE messages ADD COLUMN superseded_by INTEGER")` to a probe loop.
3. **row_to_message** — sqlite ~L1560 reads BY NAME (`r.get("superseded_by").unwrap_or(None)` — forgiving on projections that omit it). libsql ~L323 reads BY POSITION → add `superseded_by: r.get::<Option<i64>>(10)?` and **every explicit projection that feeds it must list the column in that 11th slot**.
4. **supersede()** — new trait method (default region near `set_message_priority` L714); sqlite impl near L4889; libsql impl near L4097.
5. **read-path filters** — sqlite `inbox`/`inbox_since`/`peek_oldest_unread`/`unread_count_conn` (L2945, L3046, L1746) and the libsql equivalents (L1468, plus its `unread_count_tx`). Add `AND superseded_by IS NULL` to the unread/inbox WHERE clauses.

## The migration / backward-compat approach
Strictly additive, following the established **guarded `ALTER TABLE ADD COLUMN` with NULL default** template already used for `messages.in_reply_to` (threading), `messages.idempotency_key`/`trace_id` (WL-026), `messages.priority` (WL-031), `peers.birth_cert` (WL-018), and `asks.parent_id` (WL-002/P2). SQLite `ADD COLUMN` is O(1); existing rows read `superseded_by = NULL` (== not superseded), so a pre-WL-037 DB and an old binary both behave byte-identically. The column is **nullable with no `NOT NULL`/DEFAULT** (unlike `priority`, which needed `DEFAULT 'normal'`) because NULL is the meaningful "not superseded" value. `#[serde(default)]` on the new `Message` field keeps older JSON payloads deserializable (the `in_reply_to`/`priority` precedent). Each migration step is `column_exists`-guarded (sqlite) / `pragma_table_info` probe-guarded (libsql) so re-running is a no-op.

## Store API
Add ONE method (chosen over a full new `send`-variant to keep `send` stable and follow the `set_message_priority` post-stamp precedent that the CLI/MCP already use):

```rust
/// WL-037: mark `old_id` as superseded by `new_id`. Stamps
/// messages.superseded_by = new_id on the predecessor. Authorization:
/// only the ORIGINAL SENDER of old_id may supersede it (same-identity check)
/// — a no-op error otherwise. Both ids must exist and belong to the same
/// sender; superseding an already-superseded message re-points the link
/// forward (chain). Never injects, never touches reads.
fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()>;
```

Behavior:
- Look up `old_id`'s sender; **reject (bail) if `caller != sender`** (identity/authorization — see below). Reject if `old_id` or `new_id` does not exist. Optionally require `new_id`'s sender == caller too (the replacement is the caller's own message — it always is, since main/mcp pass the just-sent `mid`).
- `UPDATE messages SET superseded_by = ?new_id WHERE id = ?old_id` (parameterized).
- Chains: superseding message B (which already supersedes A) with C simply stamps B.superseded_by = C; A→B→C is the chain. "Latest" = walk forward until `superseded_by IS NULL` (read-side helper, optional; unread filter already hides every non-tail link).

`send` itself is **unchanged** — main.rs/mcp.rs send normally then post-stamp via `supersede`, exactly as they post-stamp `set_message_priority` today.

## Read semantics for superseded messages — DECISION
**Hide-from-unread, flag-in-history (default).** Concretely:
- **`inbox` (unread + include_read), `inbox_since`, `peek_oldest_unread`, `unread_count`** → add `AND superseded_by IS NULL`. A reader never sees a stale message as a fresh unread; if the sender supersedes before the recipient drains, only the successor surfaces. This is the safety-critical path (drives nudges/wake) and is the atm-core "readers see the latest" intent.
- **`history`, `thread`, `search`, `export`** → KEEP superseded rows (auditability) but the mapper now populates `Message.superseded_by`, so the CLI/MCP renderers can mark them `[superseded by #N]`. History is a deliberate full record; hiding there would lose the audit trail.
- Backward-compatible: every existing row has `superseded_by IS NULL`, so all these filters are no-ops on a legacy store and on any deployment that never supersedes.
- **Broadcasts**: a broadcast message CAN be superseded (the stamp is per-message, orthogonal to the per-reader `reads` table). The unread filter `AND superseded_by IS NULL` applies uniformly, so a superseded broadcast drops out of every reader's unread set at once. (Note in docs; add a test.)

## Identity / authorization
atm-core's trust model is advisory `from`, but weave already enforces same-identity guards elsewhere (e.g. `clear` only marks the caller's own inbox; birth-cert takeover protection). **Plan: enforce caller == original sender of `old_id` in `supersede`.** A peer may only supersede its OWN messages. This is a cheap, parameterized lookup (`SELECT sender FROM messages WHERE id=?`) and prevents a hostile session from hiding another agent's message from inboxes (a censorship/DoS vector). Until signed identity (`sign` feature) makes `from` unforgeable this is best-effort, same caveat as the rest of weave — state that in the doc-comment. Reject (not silently no-op) so the caller learns the supersede failed.

## CLI + MCP surface
- **CLI**: `Cmd::Send` gains `#[arg(long)] supersedes: Option<i64>` (mirrors the existing `--priority`/`--idempotency-key` optional flags on `Send`, L224-226). Handler: after the normal `store.send(...)` + priority stamp (~L2896), `if let Some(old)=supersedes { store.supersede(&from, old, mid)?; }`, then print `sent #{mid} (supersedes #{old})`. No new subcommand needed. (Notify deliberately NOT extended — supersede is a messaging/inbox concept; keep scope tight, note as an open question if atm-core supersedes notifies.)
- **MCP**: extend `tool_send` (`weave-mcp/src/mcp.rs` ~L587-653) to read `args.get("supersedes").and_then(|v| v.as_i64())` and post-stamp `store.supersede(&from, old, mid)` right after the `set_message_priority` stamp (L651-653). Add `"supersedes":{"type":"integer","description":"Optional message id this message replaces; the prior message is marked superseded and hidden from unread inbox."}` to the `weave_send` inputSchema in `tool_catalog()` (~L3158-3168). **Zero standing-token cost**: the standing surface is the single `weave` meta-tool (WL-050/051); `weave_send` lives only in `tool_catalog()`, so extending its schema does not touch `MAX_STANDING_TOOLS_BYTES`. No new standing tool, no new dangerous-tool entry (it rides existing `weave_send`).

## Test layers required (docs/TESTING.md §8)
- **Unit (store, both backends)** in `weave-core/src/store.rs` `#[cfg(test)]`:
  - `supersede_stamps_predecessor`: send A, send B, `supersede(sender,A,B)`, assert A.superseded_by==B via a read.
  - `superseded_message_hidden_from_unread`: send A→recipient, supersede A with B, assert recipient `inbox`/`unread_count` returns B not A.
  - `history_retains_superseded_with_flag`: assert `history` still returns A and its `superseded_by` is populated.
  - `supersede_chain`: A→B→C, assert only C is unread.
  - `supersede_rejects_foreign_sender`: caller != A's sender → Err, A unchanged (authorization invariant).
  - `supersede_rejects_missing_id`: non-existent old/new id → Err, no panic.
  - `supersede_broadcast_drops_from_all_readers`: broadcast A, supersede with B, two readers each see only B.
  - Migration round-trip: open a DB lacking the column (simulate by the `column_exists` guard), call migrate twice, assert idempotent + column present (mirror existing priority/idempotency migration tests).
- **Integration** `weave/tests/integration.rs` (drives the compiled binary): `weave send --to r --body A`; `weave send --to r --body B --supersedes <A_id>`; assert `weave inbox --me r` shows B only and `weave history` flags A as superseded. Run is backend-parametrized by the existing CI matrix (sqlite + libsql).
- **MCP** `McpServer` test in `weave-mcp/src/mcp.rs` tests: call `weave_send` with `supersedes`, assert success + that a subsequent `weave_inbox` hides the predecessor; plus the **failure path** (supersedes a foreign/nonexistent id → error string, surfaced through the meta-tool `mode:"call"`).
- **Standing-budget guard**: confirm `standing_mcp_surface_is_within_token_budget` / `progressive_default_surface_is_just_the_meta_tool` still pass (no new standing tool) — no new test, but the guardian must re-run them.
- **Security** `weave/tests/security.rs`: superseding another identity's message is rejected (censorship/DoS guard); oversized/negative `--supersedes` is rejected; all SQL parameterized (no shell, no string-built SQL).
- **Proptest** (if a property emerges): "after any sequence of supersede stamps, exactly one message per chain has `superseded_by IS NULL` and only it is unread" — a clean invariant analogous to the ask-state monotonic proptest. Recommended but optional.

## Docs to sync
- `CHANGELOG.md` `[Unreleased]`: "feat(store): message supersede/successor chains (WL-037)".
- `README.md`: `weave send --supersedes` usage + the supersede-vs-reply distinction.
- `ARCHITECTURE.md`: messages schema note (new `superseded_by` column), the read-semantics rule (hidden from unread, flagged in history), and that supersede is replacement vs `in_reply_to` threading.
- `CONTRIBUTING.md`: only if a new invariant phrasing is added (likely not — rides existing rules).

## Edit order (dependency-respecting)
1. `weave-core/src/model.rs`: add `Message.superseded_by` (model layer first; everything depends on it).
2. `weave-core/src/store.rs`: SCHEMA column, `migrate()` guarded ADD COLUMN, `row_to_message` by-name read, `supersede()` impl, read-path `AND superseded_by IS NULL` filters, trait method decl.
3. `weave-core/src/store_libsql.rs`: mirror SCHEMA, migrate probe-loop entry, **positional `row_to_message` index 10 + every explicit projection**, `supersede()` impl, read-path filters.
4. Build/test BOTH backends green before moving up a layer.
5. `weave-mcp/src/mcp.rs`: `tool_send` post-stamp + `tool_catalog()` `weave_send` schema property.
6. `weave/src/main.rs`: `Cmd::Send` `--supersedes` flag + handler post-stamp.
7. Tests (unit → integration → mcp → security → optional proptest).
8. Docs (CHANGELOG, README, ARCHITECTURE).

## Invariants in scope
- **Parameterized SQL only** — `supersede` UPDATE and the sender lookup use bound `params!`; the read filters add only the constant literal `AND superseded_by IS NULL` (no user data). (store.rs, store_libsql.rs)
- **No shell** — supersede is pure DB; no external process. (store)
- **Input caps / id validation** — `--supersedes`/`supersedes` is an i64 message id; reject non-positive ids before bind (the `in_reply_to <= 0` precedent in mcp.rs L1652). (main.rs, mcp.rs)
- **Destructive-op / authorization** — supersede mutates another row's visibility, so enforce caller==original sender (censorship guard). (store)
- **token-light MCP surface** — extend `weave_send` catalog schema only; add NO standing tool; budget test must stay green. (mcp.rs)
- **stdout discipline** — MCP supersede emits only JSON-RPC frames; any diagnostics to stderr. (mcp.rs)
- **Dual-backend parity** — both backends compile + pass; the libsql positional-mapper trap is the highest-risk item. (store_libsql.rs)

## Risks / open questions
- **libsql positional mapper (highest risk):** libsql's `row_to_message` is index-based and inbox/history use explicit `SELECT id,ts,...,priority` projections (NOT `SELECT *`). Every such projection must append `superseded_by` as the 11th column and the mapper must read index 10, or libsql silently mis-maps. The sqlite side (`SELECT *` + by-name) is forgiving; do NOT let that mask a libsql break — run the libsql test suite.
- **FTS interaction:** `messages_fts` indexes body/subject/sender only; `superseded_by` is not indexed and `search` SELECTs full rows by id, so search still finds superseded messages (acceptable — search is an audit surface; mapper flags them). No FTS trigger change needed. Confirm in a test.
- **Chain semantics:** decide whether superseding an already-superseded message is allowed (recommended: yes — re-point forward, forming A→B→C). Unread filter handles chains for free (only the tail is NULL). A cycle is impossible if ids are monotonic and a message only supersedes an OLDER id — consider asserting `new_id > old_id` to forbid cycles.
- **Cross-identity / broadcast:** plan enforces same-sender; broadcast supersede works via the per-message stamp. Confirm atm-core does not allow an orchestrator to supersede others' messages — if it does, gate that behind orchestrator role (defer; note here).
- **Notify/cross-store:** scope kept to `send`. Open question: should `notify` and Tier-2 `outbox` intents also carry supersede? Deferred unless atm-core parity requires it — `outbox` has no `superseded_by` column in this plan.
- **`reply` coexistence:** a message can be BOTH a reply (`in_reply_to`) and a supersede target; the columns are independent. A test should send a reply, then supersede it, and assert thread + unread both behave.
