# WL-038 — Ephemeral messages with TTL + auto-sweep (atm-core parity)

Plan target: worktree `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch` off `origin/develop`).
Status: ready for implementer. Does NOT implement.

## Goal

A sender may mark a message **ephemeral** by attaching a TTL in seconds. weave stamps a nullable
`messages.expires_at = ts + ttl`. Before that instant the message behaves exactly like a normal message
(unread inbox, nudge, history, thread, search, export, inbox_since). At/after `expires_at` it is
**deleted** (delete-on-sweep, the true "ephemeral" semantics) and is excluded from every read surface.
Expiry is enforced two ways: (1) folded into the existing `gc()` retention pass, and (2) a new
`sweep_expired_messages()` Store method called **opportunistically** before unread/read surfaces so
expiry holds even with no explicit `gc`. The feature is additive (nullable column, `None`/`NULL` ==
non-ephemeral, the `superseded_by`/WL-037 precedent) and must carry through the cross-store
Intent/outbox→pull path (the `priority`/WL-031 precedent). Recommendation adopted: **delete-on-sweep,
not filter-then-sweep** — an ephemeral message that has expired must not be reconstructable.

This is the exact analogue of WL-037 (additive nullable `messages` column, both backends, hide/exclude
from read surfaces) crossed with the lease auto-sweep precedent (`sweep_expired_leases`,
`store.rs:4948`). Build the plan on those two precedents.

## Design decisions (resolve before coding — all recommended values chosen)

1. **Column = `expires_at INTEGER` (nullable, absolute epoch secs), NOT a raw `ttl`.** Storing the
   absolute deadline (`ts + ttl`) makes every sweep a single `WHERE expires_at <= now()` and matches
   `leases.expires` / `sweep_expired_leases`. The `ttl` (relative secs) is the CLI/MCP/Intent input;
   the store converts once at write. This mirrors how leases store `expires`, not `ttl`.
2. **Write path = post-insert stamp, NOT a widened `send()` signature.** `Store::send` is object-safe and
   shared; WL-031 added priority via a separate `set_message_priority(id, priority)` post-stamp and
   WL-037 added supersede via `supersede(...)`. Follow that exactly: add
   `set_message_expiry(&self, id: i64, expires_at: i64) -> Result<()>`. Do NOT touch the `send` trait
   signature (keeps both backends + every call site stable).
3. **Delete-on-sweep, not filter.** `sweep_expired_messages` and the `gc()` addition DELETE expired
   rows (and their `reads`, mirroring the existing gc `reads` prune). Read surfaces additionally carry a
   `(expires_at IS NULL OR expires_at > now)` guard so a row that is expired-but-not-yet-swept is still
   invisible between sweeps (belt-and-suspenders; the opportunistic sweep makes the window tiny).
4. **TTL cap = `MAX_MSG_TTL_SECS`.** Reuse the lease precedent value `MAX_LEASE_TTL_SECS = 86_400`
   (24h) as the model for a new `pub const MAX_MSG_TTL_SECS: i64 = 86_400;` in `model.rs`. TTL must be
   `>= 1` and `<= MAX_MSG_TTL_SECS`; reject `0`/negative/over-cap with a clear error at the CLI/MCP seam
   (the `--priority`/`idempotency_key_valid` validation precedent). Open question Q1 below if owner wants
   a longer ceiling — default to 86_400 unless told otherwise.
5. **Cross-store carry = outbox column + Intent field + pull post-stamp.** Mirror priority exactly:
   add `outbox.ttl INTEGER NOT NULL DEFAULT 0` (0 == no TTL, the priority='normal' default analogue;
   outbox is the *relative* ttl since the receiver re-stamps `ts` on commit), an `Intent.ttl` field, and
   in the pull-commit loop (`store.rs:2698-2710`) post-stamp `set_message_expiry(mid, now()+ttl)` when
   `ttl > 0` — directly beside the existing `set_message_priority` post-stamp.

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/model.rs` | model (no I/O) | Add `pub expires_at: Option<i64>` to `Message` (with `#[serde(default)]`, the `superseded_by` precedent at model.rs:124). Add `pub ttl: i64` (`#[serde(default)]`, default 0) to `Intent`. Add `pub const MAX_MSG_TTL_SECS: i64 = 86_400;`. Add a pure helper `pub fn ttl_valid(ttl: i64) -> bool { (1..=MAX_MSG_TTL_SECS).contains(&ttl) }` and/or `pub fn expiry_from_ttl(ts: i64, ttl: i64) -> i64 { ts.saturating_add(ttl) }`. | Core shape + cap + the unit-testable pure helper (TESTING §8 pure-logic layer). |
| `weave-core/src/store.rs` | store (default sqlite) | (a) SCHEMA: add `expires_at INTEGER` as the trailing `messages` column (after `superseded_by`) and `ttl INTEGER NOT NULL DEFAULT 0` to `outbox`. (b) `migrate()`: two guarded `ALTER TABLE ... ADD COLUMN` steps (the WL-037 pattern at store.rs:2250). (c) `row_to_message` (1585): read `expires_at` by name `unwrap_or(None)`. (d) Positional thread projection (3598/3617) + `peek_oldest_unread_conn` projection (1791/1858): append `, m.expires_at` and read trailing positional index 11. (e) Read-surface guards: add `AND (expires_at IS NULL OR expires_at > <now>)` to `inbox` (2993/3001), `inbox_since` (3089), `history` (3044/3057), `search` (3074), `peek_oldest_unread` (1791), `sessions` unread counts if they touch messages. (f) NEW trait method `set_message_expiry` + impl (beside `set_message_priority` 4956). (g) NEW trait method `sweep_expired_messages(&self) -> Result<usize>` + impl (model it on `sweep_expired_leases` 4948 + the gc reads/messages prune): delete expired `reads` then `messages` in one IMMEDIATE tx. (h) `gc()` (3215): add an `expires_at IS NOT NULL AND expires_at <= now()` delete to the existing tx (reads + messages), the WL-016/P6 fold-into-gc precedent. (i) `enqueue_intent` (3644): add `ttl: i64` param, bind into the new `outbox.ttl` column; `list_outbox`/`row_to_intent` projections: append `ttl`. (j) Pull-commit loop (2698-2710): post-stamp `set_message_expiry` when `intent.ttl > 0`. (k) Opportunistic sweep: call `sweep_expired_messages()` at the top of unread/read entry points (`inbox`, `peek_oldest_unread`, `inbox_since`) — best-effort, `let _ =`. | The bundled default backend. The Store trait + SQL change is the dual-backend boundary. |
| `weave-core/src/store_libsql.rs` | store (libsql, feature-gated) | Mirror EVERY item above: SCHEMA `expires_at` trailing on `messages` + `outbox.ttl` (line 76 region / outbox schema); migration loop add two rows (the WL-037 entry at 1051 + an outbox entry beside 1049); `row_to_message` positional index 11 (329-344, currently superseded_by at 10); the 11+ explicit `SELECT id, ts, ... superseded_by` projections (1505/1513/1561/1576/1597/1618/1950/4394) each gain a trailing `, expires_at`; `row_to_intent` + `list_outbox`/`enqueue_intent` gain `ttl`; `gc()` (1795) tx adds the expired-message delete; `set_message_expiry` impl beside `set_message_priority` (4156); `sweep_expired_messages` impl; pull-commit `set_message_expiry` post-stamp; opportunistic sweeps. **The libsql projection is positional — `expires_at` MUST be the trailing 12th column (index 11) in every message projection, exactly as `superseded_by` is the trailing 11th (index 10) today.** | Mutually-exclusive `--features libsql` backend; CI-gated, must compile+pass. |
| `weave/src/main.rs` | main (bin) | `Cmd::Send` clap variant (199): add `#[arg(long)] ttl: Option<i64>`. In the `Send` handler `None =>` local branch (2935-2946): after `send` + the `set_message_priority` post-stamp, if `Some(t) = ttl` validate via `model::ttl_valid` (bail on out-of-range) and `store.set_message_expiry(mid, model::expiry_from_ttl(now, t))`. In the `Some(store_path) =>` cross-store branch (2899-2933): pass `ttl.unwrap_or(0)` (validated) into the new `enqueue_intent` ttl param. Also wire `--ttl` into `Cmd::Notify`/`Cmd::Reply` send handlers (2988/3043 region) for parity if those carry priority today (they do — they call `set_message_priority`). | CLI surface (TESTING §8 CLI layer → integration.rs). NOTE: `--ttl` already exists on `lease reserve` (integration.rs:338) — no collision; this is a *different* subcommand. |
| `weave-mcp/src/mcp.rs` | mcp | `tool_send` (591 region): read optional `ttl` arg (`args.get("ttl").and_then(as_i64)`), validate via `model::ttl_valid`, post-stamp `set_message_expiry` beside the existing priority post-stamp (670); pass into `enqueue_intent` on the cross-store branch (631-643). Mirror into `tool_notify`/`tool_reply` (849/928 region) which already post-stamp priority. **Catalog only — `tool_catalog()` (3220): add `"ttl":{"type":"integer","description":"..."}` to the `weave_send` (3224) and `notify`/`reply` schema property bags. Do NOT add a standing tool** (ADR-0003 token-light invariant) — the standing surface stays the single `weave` meta-tool; the budget test guards it. | MCP tool layer (TESTING §8 McpServer layer). Zero standing-token cost = invariant compliance. |

## Dual-backend? — YES

Every `messages`/`outbox` SQL + Store-trait change crosses the boundary. Mirror points (must stay in lockstep):

| Concern | `store.rs` (sqlite, default) | `store_libsql.rs` (`--features libsql`) |
|---|---|---|
| SCHEMA `messages.expires_at` trailing | 1372-1385 region | ~line 76 messages CREATE |
| SCHEMA `outbox.ttl` | 1416-1428 | outbox CREATE |
| Migration ADD COLUMN (guarded) | 2250-2252 pattern | 1047-1058 loop (add 2 rows) |
| `row_to_message` expires_at | 1585-1607 (by-name) | 329-345 (positional index 11) |
| Explicit message projections | 1791, 3598 | 1505/1513/1561/1576/1597/1618/1950/4394 |
| `set_message_expiry` impl | beside 4956 | beside 4156 |
| `sweep_expired_messages` impl | model on 4948 + gc | mirror |
| `gc()` expired-message delete | 3215-3241 tx | 1795-1840 tx |
| `enqueue_intent` ttl + `outbox.ttl` bind | 3644-3666 | mirror enqueue_intent |
| `row_to_intent` + `list_outbox` ttl | 3668-3679 | row_to_intent 350+, list_outbox |
| Pull-commit post-stamp | 2698-2710 | mirror pull loop |
| Read-surface expiry guard | inbox/history/search/inbox_since/peek | same set |

A `compile_error!` in `weave/src/main.rs` already forbids both backends at once — no change there, just both must build/test independently.

## Invariants in scope

- **Parameterized SQL** (store.rs, store_libsql.rs): every new bind (`expires_at`, `ttl`, `now()` cutoff) goes through `params!`/`params(vec![...])`. The only constant literals are the additive DDL identifiers (the `superseded_by`/lease-sweep precedent). No user value is ever string-interpolated into SQL.
- **No-shell argv-only** (main.rs, mcp.rs): unaffected — TTL is a numeric arg, never reaches a process spawn. Confirm the WL-036 post-send hook path still only exports message fields as env (ttl is not a new shelled value).
- **Input caps** (model.rs, main.rs, mcp.rs): `MAX_MSG_TTL_SECS` is the new cap; `ttl_valid` rejects `<=0` and `> MAX_MSG_TTL_SECS` at BOTH the CLI and MCP seams (defense in depth — the lease/idempotency-key precedent). Prevents an overflow/abuse where `ts + ttl` could wrap (`expiry_from_ttl` uses `saturating_add`).
- **stdout discipline in MCP** (mcp.rs): no new stdout writes; only the JSON-RPC result string changes. Any diagnostic stays on stderr.
- **token-light MCP surface / ADR-0003** (mcp.rs): new capability is exposed ONLY through `tool_catalog()` schema properties on existing tools — NOT a new standing tool. The `standing_mcp_surface_is_within_token_budget` test must still pass (it will, since no standing tool is added).
- **Additive/backward-compatible** (model.rs, both stores): nullable column, `#[serde(default)]`, `NULL`/`None`/`0`-ttl == legacy behavior; old DBs migrate in place (O(1) `ADD COLUMN`); old JSON payloads (Message/Intent) still deserialize.

## Test layers required (TESTING.md §8 — one per surface, plus the security/prop layer)

1. **Pure logic → unit (`weave-core/src/model.rs` `#[cfg(test)]`)**
   - `ttl_valid`: rejects 0, negative, `MAX_MSG_TTL_SECS + 1`; accepts 1 and `MAX_MSG_TTL_SECS`.
   - `expiry_from_ttl`: `expiry_from_ttl(ts, ttl) == ts + ttl`; `i64::MAX` ts saturates (no panic/wrap).

2. **Store unit (both backends — `store.rs` AND `store_libsql.rs` `#[cfg(test)]`)** — mirror the WL-037 `history_retains_superseded_with_flag` (store.rs:10156) twin-test pattern:
   - `expiry_stamps_and_excludes_from_unread`: send, `set_message_expiry(id, now()-1)` (already expired), assert it is absent from `inbox`/`peek_oldest_unread`/`inbox_since`/`history`/`search` AND the opportunistic sweep deleted the row (`total_messages` drops).
   - `sweep_expired_messages_deletes_expired_keeps_live`: one expired + one future-expiry + one non-ephemeral; assert sweep returns 1 and only the expired row + its `reads` are gone.
   - `gc_also_reaps_expired_ephemeral`: an expired-but-newer-than-retention message is removed by `gc()` even though `ts >= cutoff`.
   - `non_ephemeral_message_is_never_swept`: `expires_at IS NULL` survives sweep + gc.
   - migration test: open a legacy-shaped DB (or assert `column_exists`/pragma) confirms `messages.expires_at` + `outbox.ttl` are added idempotently (the WL-037 migration test precedent).

3. **CLI → `weave/tests/integration.rs`** (drives `CARGO_BIN_EXE_weave`):
   - `send_ttl_message_expires_from_inbox`: `weave send --ttl 1 ...`, assert recipient sees it, then (deterministically — inject an already-past expiry via a small `--ttl` and a sweep-triggering read, NOT a real sleep) assert it vanishes from `weave inbox`/`weave history`. Prefer a 1s ttl + an explicit `weave gc` call to force the sweep deterministically rather than wall-clock sleeping.
   - `send_ttl_rejects_out_of_range`: `--ttl 0` and `--ttl 999999999` exit non-zero with the cap message.
   - `send_ttl_cross_store_carries_through`: `weave send --to-store <other> --ttl N`, pull on the other store, assert the committed message carries the expiry.

4. **MCP → `McpServer` test in `weave-mcp/src/mcp.rs` (or the mcp integration test)**: 
   - `weave_send {ttl}` happy path stamps expiry; failure path: `ttl: 0` returns an error result (the `supersedes <= 0` failure-path precedent).
   - **Standing-surface budget**: confirm `standing_mcp_surface_is_within_token_budget` still passes (ttl added only to catalog, no new standing tool) — assert the catalog `weave_send` schema now lists `ttl`.

5. **Security / resource → `weave/tests/security.rs`**:
   - `ttl_is_capped`: an over-cap `--ttl` is rejected (no row, no panic) — the resource-bound invariant.
   - `expired_ephemeral_is_not_recoverable`: after expiry+sweep the body is absent from inbox, history, search, AND export (delete-on-sweep, not filter) — the "ephemeral means gone" security property.

6. **Proptest → `weave/tests/prop.rs`**:
   - `expiry_monotonicity`: for any `ts >= 0` and `ttl in 1..=MAX_MSG_TTL_SECS`, `expiry_from_ttl(ts, ttl) > ts` (strictly, no wrap) and `<= ts + MAX_MSG_TTL_SECS`.
   - (optional) `sweep_idempotent`: sweeping twice removes the same set; a message with `expires_at > now` is never swept.

## Docs to sync (ship WITH the code — guardian blocks on docs-fork)

1. **`CHANGELOG.md`** — under `## [Unreleased]` `### Added`, a bullet in the exact WL-037 style already present (lines 16-24): name it **"Ephemeral messages with TTL (WL-038), `weave send --ttl <secs>` (CLI) and a `ttl` property on `weave_send` (MCP, zero standing-token cost)"**; note: additive nullable `messages.expires_at` (both backends), delete-on-sweep via `gc()` + new `sweep_expired_messages` (opportunistic), `MAX_MSG_TTL_SECS = 86400` cap, carries through the cross-store intent/outbox→pull path (`outbox.ttl`).
2. **`docs/REPOWIRE-PARITY.md`** — add a matrix row beside the supersede row (line 62) and/or the numbered SUPERSET list: **"Ephemeral messages / TTL auto-sweep (atm-core)"** → `weave send --ttl <secs>` / `weave_send {ttl}` → ✅ HAVE → **WL-038**; additive `messages.expires_at` (both backends), delete-on-sweep (gc + opportunistic), TTL-capped, cross-store via `outbox.ttl`.
3. **`ARCHITECTURE.md`** — in the message/store-schema section that documents `messages` columns and the gc/retention + lease-sweep behavior: document `expires_at` (nullable, absolute epoch), the dual-enforcement model (gc fold-in + opportunistic `sweep_expired_messages`), delete-on-sweep rationale, and the read-surface exclusion. If there is an injector/read-surface table, note ephemeral exclusion from all read surfaces and that broadcasts may also be ephemeral.
4. **`README.md`** — IF user-facing CLI is documented there (it is — `weave send` flags): add `--ttl <secs>` to the `weave send` usage with a one-line note ("ephemeral: auto-deleted after N seconds; capped at 24h").
5. **`docs/TESTING.md`** — if it maintains a per-WL test inventory (§8 / the parity test list), add the WL-038 test cases above so the inventory stays drift-free.
6. **`CONTRIBUTING.md`** — no change expected (no new module, no new invariant *category*; the TTL cap is a new constant under the existing input-cap invariant). Only touch if it enumerates per-column schema.

## Edit order (dependency-respecting)

1. **`model.rs`** — `Message.expires_at`, `Intent.ttl`, `MAX_MSG_TTL_SECS`, `ttl_valid`, `expiry_from_ttl` + their unit tests. (Lowest layer; everything else depends on these.)
2. **`store.rs`** — SCHEMA + migration; `row_to_message` + every projection; `set_message_expiry`; `sweep_expired_messages`; `gc()` fold-in; read-surface guards; opportunistic sweeps; `enqueue_intent`/`outbox.ttl`/`row_to_intent`/`list_outbox`; pull-commit post-stamp. Add store unit tests. Build+test default backend green.
3. **`store_libsql.rs`** — mirror item-for-item (positional projections: `expires_at` trailing index 11). Build+test `--features libsql` green. (Do NOT proceed until both backends compile+test.)
4. **`weave/src/main.rs`** — `--ttl` on `Send` (and `Notify`/`Reply` for parity), validation, local post-stamp + cross-store pass-through. Add integration tests.
5. **`weave-mcp/src/mcp.rs`** — `tool_send`/`tool_notify`/`tool_reply` ttl read+validate+post-stamp+cross-store; `tool_catalog` schema properties. Add McpServer tests; re-confirm standing-budget test.
6. **`weave/tests/security.rs` + `weave/tests/prop.rs`** — cap, non-recoverability, monotonicity.
7. **Docs** (CHANGELOG, REPOWIRE-PARITY, ARCHITECTURE, README, TESTING) — in the SAME commit/PR as the code.
8. Full gate: `cargo build --release`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`, then the libsql trio (`clippy/build/test --no-default-features --features libsql`) and the `sign` + `libsql+sign` combos.

## Risks / open questions

- **Q1 (TTL ceiling):** Plan uses `MAX_MSG_TTL_SECS = 86_400` (24h), mirroring `MAX_LEASE_TTL_SECS`. If the owner wants ephemeral messages to live longer (days), bump this one constant — flagged but not blocking. Most-architecture-consistent default chosen.
- **Q2 (broadcast ephemerality):** A broadcast is persisted-not-injected with per-reader `reads`. An ephemeral broadcast is fine under delete-on-sweep (the row + all its `reads` are deleted together — the gc `reads` prune already handles this), but confirm the read-surface guard is applied to the broadcast path (`recipient IN (BROADCAST_SQL)`), not just direct messages. No special-casing needed; just don't miss the broadcast branch in `inbox`/`history`/`inbox_since`.
- **Q3 (deterministic expiry in CLI/integration tests):** Do NOT rely on wall-clock `sleep` for the CLI expiry test (flaky/slow). Use a 1s ttl plus an explicit `weave gc 0` (or a read that triggers the opportunistic sweep) to force deletion deterministically. Implementer must choose the deterministic path; if no CLI hook can stamp a past expiry, the store-unit tests (which call `set_message_expiry(id, now()-1)` directly) carry the precise-expiry assertions and the CLI test only asserts the round-trip + cap.
- **Q4 (opportunistic-sweep cost):** Calling `sweep_expired_messages` on every `inbox`/`peek` read adds one indexed `DELETE` per read. Bound is fine (same shape as the existing per-read read-marking tx), but if a hot path shows up, gate the sweep behind a "swept within last N secs" watermark later — out of scope for WL-038, note only.
- **Drift watch:** the libsql positional projection is the single most error-prone surface — `expires_at` must be the **trailing** column (index 11) in EVERY message `SELECT` that feeds `row_to_message`, exactly as `superseded_by` is index 10 today. The guardian should diff every `SELECT ... FROM messages` against `row_to_message`'s index list.
