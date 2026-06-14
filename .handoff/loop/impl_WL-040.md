# WL-040 — Canonical session export/import — Implementation change log

Status: implemented; both backends compile + test green; all four feature combos
build clean (sqlite default, libsql, sign, libsql+sign). NOT committed/pushed
(leader owns delivery).
Branch/worktree: `wl-038-042-batch` @ `/home/drdave/Desktop/meta/weave-wl038-042`.
Built on top of WL-038 (`messages.expires_at`) and WL-039 (`messages.kind`) —
neither disturbed (no Store/schema change in this card).

## Build / test verification

- `cargo build` (default sqlite) — clean
- `cargo build --release` — clean
- `cargo build --no-default-features --features libsql` — clean
- `cargo build --features sign` — clean
- `cargo build --no-default-features --features "libsql sign"` — clean
- `cargo clippy --all-targets -- -D warnings` (sqlite) — clean
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` — clean
- `cargo fmt --all --check` — clean
- `cargo test` (sqlite black-box) — **677 passed** (was 655 pre-card)
- `cargo test --no-default-features --features libsql` — **628 passed, 1 ignored**
- `cargo test -p weave-core session::` — 10 passed (pure unit)

## Store / backend boundary: NOT CROSSED (asserted, not assumed)

WL-040 adds **no `Store` trait method and no schema change**. Export reads through
existing `Store::history` + `Store::list_asks` (+ `total_messages` for insert/skip
accounting); import writes through existing `Store::send` (free id-remap via fresh
local autoincrement ids; idempotent dedup on `idempotency_key`). Therefore
`store.rs` and `store_libsql.rs` are **untouched** and the libSQL build needs no
mirrored edit. The integration/security tests run under both backends (they drive
the compiled binary), confirming portability is backend-agnostic.

## Files touched (rationale each)

### Code
- `weave-core/src/session.rs` **(new)** — pure (de)serialize layer: `SessionExport`
  / `ExportedMessage` / `ExportedAsk` / `ExportedMemory` serde structs;
  `FORMAT_TAG`/`SCHEMA_VERSION` consts; `serialize_session`, `to_json` (pretty,
  stable key order, trailing newline), `from_json` (validates magic + `schema_version
  <= SCHEMA_VERSION`, tolerant of unknown fields), `synth_idempotency_key`
  (`wl040:<sanitized-identity>:<id>`). **Pure — no Store, no fs, no socket.**
- `weave-core/src/lib.rs` — `pub mod session;` (unconditional, default build).
- `weave/src/session.rs` **(new)** — bin-layer I/O handler (mirrors `backup.rs`):
  `run_export` (gather messages/asks/memory, build envelope, path-guard `--out`,
  atomic sibling-temp+rename write, read-back-verify message count) and `run_import`
  (path-guard `--in`, parse via `from_json`, **bound every field BEFORE any write**,
  re-insert messages via `Store::send` with `--as` identity remap + synth dedup key,
  write memory via `memory_write`, idempotent; `--dry-run` reports counts only).
- `weave/src/main.rs` — `mod session;`; new `SessionCmd { Export{out,for_id,limit,
  force}, Import{in_path,as_id,dry_run} }` subcommand-group enum; `Cmd::Session{cmd}`
  arm + dispatch (resolves identity via `resolve_me_explicit`, the existing pattern;
  the `session` group avoids colliding with the existing top-level `Export` = WL-034
  HTML).

### Tests added (22 total)
- `weave-core/src/session.rs` (pure unit, 10): `round_trip_preserves_messages`,
  `round_trip_preserves_asks_and_memory`, `empty_session_round_trips`,
  `from_json_rejects_wrong_magic`, `from_json_rejects_future_schema_version`,
  `from_json_tolerates_unknown_fields`, `from_json_rejects_garbage`,
  `synth_key_is_deterministic_and_bounded`, `synth_key_sanitizes_hostile_identity`,
  `synth_key_differs_per_source_id`.
- `weave/tests/integration.rs` (cross-DB, 4):
  `session_export_import_round_trips_across_distinct_dbs` (headline portability:
  message sent in DB-A appears for the identity in a fresh DB-B after export→import),
  `session_import_is_idempotent_on_reimport` (re-import dedups to one copy),
  `session_import_dry_run_writes_nothing`,
  `session_export_import_round_trips_mesh_memory` (memory written in A's
  `XDG_CONFIG_HOME` is readable in B's different config home).
- `weave/tests/security.rs` (untrusted-input, 8): `session_import_rejects_oversized_body`,
  `session_import_rejects_control_char_identity`,
  `session_import_rejects_malformed_idempotency_key`,
  `session_import_stores_sql_and_shell_metachars_as_literals` (SQL/shell metachar
  body round-trips byte-identical, DB intact, no shell),
  `session_export_refuses_to_overwrite_without_force`,
  `session_export_rejects_nonexistent_parent_dir`,
  `session_import_rejects_missing_and_directory_in_path`,
  `session_import_rejects_non_weave_json`.

### Docs (shipped with the code)
- `docs/FORMAT-session-export.md` **(new)** — the interchange contract: envelope +
  every field's type/cap, idempotency-key synthesis rule, conflict/skip-existing
  policy, identity-remap (`--as`) semantics, v1 scope boundary (messages+memory
  imported; asks recorded-not-replayed; peers excluded), worked round-trip example.
- `CHANGELOG.md` — `[Unreleased] / Added` WL-040 bullet.
- `ARCHITECTURE.md` — "Three distinct export surfaces" subsection (WL-034 HTML /
  WL-035 binary snapshot / WL-040 canonical JSON) + `session.rs` added to the
  weave-core file layout.
- `docs/MULTI-SURFACE-PARITY.md` — new capability row (CLI ✅, MCP ❌-by-design).
- `docs/REPOWIRE-PARITY.md` — new casr "Session export / resume" → HAVE row.
- `README.md` — `weave session export` / `import` usage lines.
- `docs/TESTING.md` — WL-040 three-layer test note (after the WL-039 note).

## Leader scope decisions implemented (locked)

- **Subcommand group** `weave session export/import` — done (avoids the WL-034
  `weave export` collision).
- **Canonical envelope** `{weave_session_export:1, schema_version:1, identity,
  exported_at, messages[], asks[], memory[]}` with magic + version validation — done.
- **Pure core module** `weave-core/src/session.rs` (serde + to_json/from_json +
  validation, NO I/O); **bin handler** `weave/src/session.rs` (file/store I/O,
  path-traversal guards, sibling-temp+rename, read-back verify) — done.
- **MESSAGES full round-trip** via `Store::history` (export) + `Store::send` (import,
  free id-remap, dedup on idempotency_key, synth key for keyless legacy) — done. No
  new Store method, no schema change.
- **MEMORY full round-trip** via `memory_scopes`/`memory_list` (export) +
  `memory_write` (import) across all scopes — done.
- **ASKS exported for fidelity, NOT imported** — done; import prints
  "N ask(s) in archive not imported — see WL-040b".
- **PEERS excluded by design** — done (documented in ARCHITECTURE + format doc).
- **Conflict policy** skip-existing by idempotency_key; `--as` remap; idempotent
  re-run — done + proven by `session_import_is_idempotent_on_reimport`.
- **Invariants**: untrusted-input field-bounding before any bind; all writes via
  parameterized `Store::send`; no-shell; path-traversal guard on `--in`/`--out`;
  no new standing MCP tool — all upheld (see below).

## WL-040b follow-up (filed, as instructed)

**WL-040b — faithful ask-thread replay on session import.** v1 records asks in the
envelope but does NOT replay them, because faithful ask import needs a new
dual-backend `Store::import_ask` that mints/accepts a foreign correlation id and
drives `AskState` transitions out of natural order — a distinct cohesive change with
real ask-state-machine risk. Messages + memory are FULLY round-trippable today (this
is a real decomposition, not a stub). WL-040b would add the dual-backend
`import_ask` method (mirrored in both `store.rs` and `store_libsql.rs`), accept an
out-of-order `AskState`, and replay the `asks[]` block; the envelope schema already
carries the asks losslessly so it is a non-breaking, additive follow-up.

## Deviations from the plan (with reasoning)

1. **MEMORY is IN scope (full round-trip), overriding the planner's "memory OUT of
   v1".** The leader's locked scope decisions explicitly require full memory
   round-trip; the planner doc predates that decision. Implemented via the existing
   `memory_scopes`/`memory_list`/`memory_write` fns (no new memory API), with a
   `memory[]` envelope block and `(scope_kind, scope_name)` scope reconstruction.
   The `schema_version` stays at 1 since memory was present from the first shipped
   version of this format.

2. **Subject capped at import via a local `MAX_IMPORT_SUBJECT` (4096).** `Store::send`
   does not bound `subject` itself; since the import file is untrusted, I added an
   explicit subject cap in `validate_messages` so a hostile file cannot smuggle an
   unbounded subject (mirrors the body-cap discipline). Not a behavior change for
   the normal CLI/MCP send paths.

3. **`MAX_IMPORT_FILE_BYTES` (256 MiB) guard on the import file.** Belt-and-suspenders
   RAM-DoS guard before parsing an untrusted file into memory; the per-field caps
   still apply after parse. Not in the plan but consistent with the input-cap
   invariant.

4. **Insert-vs-skip accounting uses `total_messages()` before/after each `send`.**
   `Store::send` returns an existing id on a dedup hit without signaling "was this
   new?", so the handler counts the row-count delta to report inserted vs skipped
   accurately (drives the idempotency test's assertions). Read-only, both backends.

## Invariants upheld

- **Untrusted-input field-bounding (the central import invariant)** — every field
  from the JSON file is bounded BEFORE any store write: `check_ident` on the
  importing id + every per-message sender/recipient, `check_body`/`MAX_BODY` on
  bodies, subject cap, `idempotency_key_valid`/`trace_id_valid` shape checks; memory
  body cap + scope-kind validation. Proven by the 8 security tests (clean rejection,
  no partial write).
- **Parameterized SQL** — all import writes go through `Store::send` (already fully
  `params!`-bound). No new SQL literal anywhere; the envelope's identity/body reach
  SQL only as bound params via `send`.
- **No-shell** — import/export spawn no external program; no argv construction at all.
  A SQL/shell-metachar body stores as literal text (proven by
  `session_import_stores_sql_and_shell_metachars_as_literals`).
- **Path-traversal / arbitrary-write guard** — `--out` (UTF-8, overwrite-`--force`,
  parent-exists) and `--in` (UTF-8, exists, not-a-directory) copy the `backup.rs`
  discipline; the format embeds **no path fields** (only `scope_kind`/`scope_name`/
  `key`), so a crafted file cannot direct a write elsewhere. Export writes atomically
  (sibling-temp + rename) and read-back-verifies.
- **Token-light MCP surface (ADR-0003 / WL-051)** — **no new standing MCP tool, no
  `tool_catalog()` entry**; CLI is the zero-standing-cost path. The
  `standing_mcp_surface_is_within_token_budget` test is known-unaffected.
- **Destructive-op gating** — import is additive (inserts; idempotent on re-run), not
  destructive, so it needs no `confirm`. Export `--force` only overwrites the
  user-named output file, guarded.
- **Layer DAG** — pure (de)serialize in `weave-core/src/session.rs` (no I/O); all
  file/store/memory I/O in `weave/src/session.rs` (bin layer); no upward dep added.
