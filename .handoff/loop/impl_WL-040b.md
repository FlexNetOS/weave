# Implementer change log — WL-040b: faithful ask-thread replay on session import

Status: **implementation ready**. Both backends compile; sqlite + libsql test suites green; clippy (`--all-targets -D warnings`) and `fmt --check` clean on both backends. **Store/backend boundary crossed** (3 new dual-backend `Store` methods).

WL-040b is **complete** — ask_groups was implemented FULLY, not deferred. **No WL-040c blocker filed** (group reconstruction hit no irreducible conflict: `target_count` is preserved verbatim and any dangling-skipped child simply counts as `failed` on the target, which is faithful).

## Files touched

| File | Layer | Rationale |
|---|---|---|
| `weave-core/src/session.rs` | model (pure) | Extended `ExportedAsk` with `kind`/`options`/`reply_to`/`close_note`/`parent_id` (all `#[serde(default)]`); added `ExportedAskGroup` struct + `ask_groups` field on `SessionExport`; `serialize_session` takes the new arg. No `schema_version` bump (additive). Updated round-trip unit tests + a new older-export-defaults test. |
| `weave-core/src/store.rs` | store (sqlite) | Added `import_ask` / `import_ask_group` / `list_ask_groups` to the **trait**; implemented on `SqliteStore` (named `params!`, out-of-order INSERT in any `AskState`, dedup pre-check); moved `AskState`/`AskGroup` to the unconditional model-import (trait signatures need them). Store-side unit tests. |
| `weave-core/src/store_libsql.rs` | store (libsql) | Mirrored the 3 methods on `LibsqlStore` with **positional** `params(vec![...])`; the 15-column asks INSERT order matches `row_to_ask` indices 0..14. libsql-gated unit tests. |
| `weave/src/session.rs` | bin (I/O) | Export mapper carries the new ask fields + reads `ask_groups` via `list_ask_groups`. Import: builds the `source_msg_id → new_local_id` map during the message loop (captures `Store::send`'s return, which is the existing id on dedup), replays groups-then-asks with `--as` remap + parent rewire, skips danglers (counted), updates both summary strings. Added `validate_asks` + `validate_ask_groups` (untrusted-input bounding before any write). |
| `weave/tests/integration.rs` | test | `session_import_replays_ask_thread_and_group` (answered+acked thread + broadcast-ask group → fresh DB → remapped links resolve, parent linkage present, re-import idempotent); `session_import_dry_run_counts_asks_without_writing`. |
| `weave/tests/security.rs` | test | 5 new cases: dangling-ref skipped safely, malformed state rejected, oversized options rejected, malformed parent_id rejected, hostile asker rejected. |
| `docs/FORMAT-session-export.md` | doc | Respec asks (now replayed) + new `ExportedAsk` fields + `ask_groups[]` block + id-remap/dangling/idempotency/reply_to-NULL policy. |
| `docs/REPOWIRE-PARITY.md` | doc | casr "Session export / resume" row → ask-thread fidelity complete (WL-040b). |
| `docs/ARCHITECTURE.md` | doc | `import_ask`/`import_ask_group`/`list_ask_groups` Store surface + "asks replayed" note. |
| `CHANGELOG.md` | doc | `[Unreleased]` WL-040b entry. |
| `.handoff/loop/backlog.md` | doc | WL-040b → `[x] DONE` with summary. |

## Key design decisions (per the LOCKED leader scope)

1. **`Store::send` dedup contract — VERIFIED**: both backends `return Ok(id)` of the EXISTING row on an idempotency-key hit (store.rs:3046, store_libsql.rs:1485). So the value `send` returns IS the remapped local message id whether inserted or deduped — the msg-id map is built directly from it, no separate lookup needed.
2. **Ask dedup key**: regenerate the local ask id from the remapped question id (`new_ask_id(new_q)`), dedup-skip on `(asker, askee, question_msg_id)`. Robustly idempotent because the message remap is itself idempotent.
3. **`import_ask` bypasses the lifecycle**: inserts a row directly in any `AskState` (the question/answer message rows already exist from the message-import pass); does NOT touch `messages` and does NOT run `can_transition`.
4. **ask_groups — COMPLETED**: envelope carries `ask_groups` (read via new `list_ask_groups`), replayed via `import_ask_group` BEFORE children, each child's `parent_id` rewired to the freshly minted local group id. No concrete blocker.
5. **`reply_to` chain — NULLed on import** (documented): it references a regenerated source ask id; rewriting it would dangle. The thread itself replays faithfully — only the cross-ask chain pointer is dropped.
6. **Dangling ask** (question or claimed-answer message absent from export): skipped + counted, never an inserted broken link.

## Deviations from the plan

- **Plan risk #3/#4 (ask_groups / reply_to) proposed DEFER; leader scope #4 LOCKED to COMPLETE.** Implemented ask_groups fully (added `ExportedAskGroup` + `Store::import_ask_group` + `Store::list_ask_groups`). `reply_to` is NULLed (kept from plan option b) since dedup makes the new ask id deterministic but the chain still references regenerated source ids; documented.
- **Added `Store::list_ask_groups`** (not named in the plan) — required because no existing read method enumerates `ask_groups`; the export needs it to carry the parent anchors.
- **`serialize_session` signature grew** an `ask_groups` arg (`#[allow(clippy::too_many_arguments)]`).

## Test count

- **New tests: 12** (5 unit + 2 integration + 5 security).
  - Unit (sqlite, `store.rs`): `import_ask_materializes_answered_and_is_idempotent`, `import_ask_materializes_acked_and_group`, `import_ask_rejects_malformed_inputs`.
  - Unit (libsql, `store_libsql.rs`): `import_ask_materializes_answered_and_is_idempotent_libsql`, `import_ask_acked_and_group_libsql`, `import_ask_rejects_malformed_inputs_libsql`. (3)
  - Pure model (`weave-core/src/session.rs`): `older_export_without_new_ask_fields_defaults` (1 new; 2 existing round-trip tests extended with the new fields). → that's the 5th "unit" + 1.
  - Integration: `session_import_replays_ask_thread_and_group`, `session_import_dry_run_counts_asks_without_writing`. (2)
  - Security: `session_import_skips_dangling_ask_reference`, `session_import_rejects_malformed_ask_state`, `session_import_rejects_oversized_ask_options`, `session_import_rejects_malformed_ask_parent_id`, `session_import_rejects_hostile_ask_asker`. (5)
- **No new proptest** — replay is materialization, not a new monotonic invariant (the existing `AskState` lifecycle proptest still guards the normal path).

## Build/test verification

- `cargo build` (sqlite) — clean.
- `cargo build --no-default-features --features libsql` — clean.
- `cargo test` (sqlite) — **10 suites OK, 0 failed**.
- `cargo test --no-default-features --features libsql` — all suites OK, 0 failed (incl. the 3 libsql unit tests + black-box integration/security driving the libsql binary).
- `cargo clippy --all-targets -- -D warnings` (sqlite) — exit 0.
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` — exit 0.
- `cargo fmt --all --check` — exit 0.

## Note for the verifier / guardian

- Pre-existing latent warning unrelated to this change: `cargo clippy -p weave-core` (package-scoped, default features) flags `JobState` as an unused import at `store.rs:11` because all its users are `#[cfg(feature="sqlite")]`-gated functions. This is present on the clean `origin/develop` base (confirmed via `git stash`) and does **not** fire under the full-workspace `--all-targets` clippy (which is what CI runs and what passed above). Left untouched (out of scope; not introduced by WL-040b). Flagging for awareness.
