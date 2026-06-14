# 04 — Guardian review — WL-038..WL-042 combined batch

**Worktree:** `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch`, base `origin/develop`)
**Input:** `03_verifier_report.md` = **GREEN** (706 sqlite / 657 libsql / 697 libsql·sign; fmt+clippy clean on all six gated combos + surfaces; standing-MCP budget + BROADCAST drift-guard green; HOME-isolation confirmed).
**Scope reviewed:** full uncommitted diff vs `origin/develop` + new files `weave-core/src/session.rs`, `weave/src/session.rs`, `docs/FORMAT-session-export.md`.

## VERDICT: **APPROVE**

---

## Part 1 — Security/correctness invariants

### 1. No shell — OK
- No `sh -c`, no `bash -c`, no format!-built command string anywhere in the diff.
- WL-042 provider writers compose config TEXT (TOML/JSON/YAML), they do **not** spawn — confirmed (no `Command` in `setup.rs` additions).
- Only added external spawn is `Command::new("git").args(["init"])` in `weave/tests/integration.rs:4081` (WL-041 git-hook test fixture) — argv-only, test code. OK.

### 2. Parameterized SQL — OK
- Every new query binds values with `params!`/`params(vec![…])`: `expires_at`/`kind` filters use `?N` (`store.rs` unread/peek/inbox/history/search/inbox_since/thread; `store_libsql.rs` mirrors at the same indices), `set_message_expiry`, `sweep_expired_messages`, `supersede_prior_idle`, `enqueue_intent` ttl carry, `commit_pulled` re-stamp.
- No **added** `format!`-built SQL. The pre-existing `format!(… {bc} …)` lines interpolate only `BROADCAST_SQL` (compile-time const); the migration probe interpolates `{table}`/`{col}` from a hardcoded static list (pre-existing pattern, no user data). BROADCAST drift-guard test green (verifier §3). OK.
- `KIND_IDLE` is bound as a param (`crate::model::KIND_IDLE`), not inlined. OK.

### 3. Layer DAG — OK
- `weave-core/src/session.rs` is **pure** (no DB / fs / socket; doc-asserted and code-confirmed) — correctly in `weave-core`.
- `weave/src/session.rs` holds all file+store+memory I/O — correctly in the bin layer; imports only downward (`weave_core::*`). No upward dep introduced. `pub mod session;` added to `weave-core/lib.rs` + `mod session;` to `weave/main.rs`. OK.

### 4. Paste-safe injection — N/A
- No mux/injector arm touched in this batch.

### 5. Input caps — OK
- WL-038: `MAX_MSG_TTL_SECS = 86_400`, `ttl_valid(1..=cap)` enforced at **both** seams (CLI `main.rs:210`, MCP `mcp.rs` weave_send/notify/reply) before any write; `expiry_from_ttl` uses `saturating_add` (overflow-safe). `ttl: 0` rejected (test `weave_send_ttl_zero_is_rejected`).
- WL-040 import bounds EVERY field before any store write (`validate_messages`/`validate_memory`): `check_ident` sender+recipient, `check_body`/`MAX_BODY`, `MAX_IMPORT_SUBJECT=4096`, `idempotency_key_valid`, `trace_id_valid`, scope-kind whitelist, `MAX_IMPORT_FILE_BYTES=256MiB`. `synth_idempotency_key` sanitizes a hostile source identity to `[A-Za-z0-9_]` (SQL/metachar smuggle test passes). Format carries no path fields → no in-payload traversal; `--in`/`--out` UTF-8+existence+overwrite guarded.

### 6. Destructive ops gated — OK
- WL-038 sweep/gc delete **only** genuinely-expired rows (`WHERE expires_at IS NOT NULL AND expires_at <= now()`), reads-then-messages in one IMMEDIATE tx; `expires_at IS NULL` (permanent) is never touched (test `non_ephemeral_message_is_never_swept`). Bounded by the existing gc — no new unbounded sweeper.
- WL-039 `supersede_prior_idle` is sender-scoped (`sender = ?` authz, the WL-037 spine), `kind='idle'`-scoped (never touches a real message — tested), unread-scoped, `id <> new_id` (idempotency replay = no-op). Reuses the `superseded_by` hide-spine; never deletes, never touches `reads`.
- WL-040 import is idempotent (idempotency-key dedup; re-run = no-op).
- WL-041/042 never clobber foreign config: read-existing → merge-own-entry-only → atomic temp+rename + one-time `.weave.bak` (0o600) → **read-back verify** that weave's entry landed AND every foreign entry survived (a non-NotFound read error aborts without writing). Confirmed in `merge_hooks_at`/`prune_hooks_at`/`merge_codex_notify`/`merge_aider_stanza` + `verify_*` predicates + `backup.rs::verify_restored_bytes`.

### 7. MCP stdout discipline — OK
- No new stdout writes in `mcp.rs`; new `ttl`/`dedupIdle` are handler params returning protocol frames. Logging discipline untouched.

### token-light MCP surface — OK
- WL-038 `ttl` and WL-039 `dedupIdle` are **catalog-only** (added to existing `weave_send`/`weave_notify`/reply op input schemas in `tool_catalog()`), **no new standing tool**. `standing_mcp_surface_is_within_token_budget` green (verifier §2). WL-040/041/042 are CLI-only (zero MCP surface). OK.

---

## Part 2 — Rust-native drift guard

- **No Cargo.toml / Cargo.lock change** in the entire diff → **zero new dependencies, zero new crates**. Default-feature `cargo tree` unchanged by construction. OK.
- WL-042 provider writers are **Rust-native by hand**: codex TOML via line-based merge (**no `toml` dep**), gemini JSON reuses the existing serde_json hook merge, aider YAML via manual single-quoted-scalar compose (**no `serde_yaml` dep**). Confirmed in `setup.rs`.
- The `.codex`/`.gemini`/`.aider` files weave writes are **OUTPUT sidecar artifacts for the user**, NOT build/runtime inputs to weave — the allowed sidecar case. Nothing in `src/`/`Cargo.toml`/`build.rs`/CI builds against them; weave is not expected to mirror them by hand. The README/parity docs explicitly note the codex writer is Rust-native and "NOT via the ecc `.codex` sidecar." No drift.
- New module `session` lives inside the existing `weave-core` crate (no new crate — honors the "do not add crates" interim rule).
- **No misinformation drift:** the gemini/aider scaffolds are documented as **scaffold-with-caveat** (UNCONFIRMED/LIMITED) in README, MULTI-SURFACE-PARITY, REPOWIRE-PARITY, the CLI output, AND the in-code comments — consistent, not a false claim of confirmed support.

---

## Part 3 — Docs sync

All user-facing surfaces updated **with** the code (no code↔docs fork):
- **CHANGELOG.md** `[Unreleased]` — all five cards (WL-038 ephemeral/ttl, WL-039 idle dedup, WL-040 session export/import, WL-041 read-back, WL-042 multi-provider). OK.
- **README.md** — provider table (claude/codex/gemini/aider) with confirmed/partially/unconfirmed/limited caveats; `--ttl`, idle dedup, `session export/import`, read-back. OK.
- **ARCHITECTURE.md** — provider, ttl/ephemeral, idle, WL-040 interchange, read-back. OK.
- **docs/REPOWIRE-PARITY.md** / **docs/MULTI-SURFACE-PARITY.md** — casr parity rows for providers + session resume + read-back. OK.
- **docs/SECURITY.md** — WL-041 read-back verification section. OK.
- **docs/TESTING.md** — WL-038/039/040 test-layer entries. OK.
- **docs/FORMAT-session-export.md** (new, 167 lines) — byte-consistent with `weave-core/src/session.rs` + `weave/src/session.rs`: `FORMAT_TAG=1`, `SCHEMA_VERSION=1`, `MAX_BODY=65536`, `MAX_IMPORT_*` caps, idempotent-reimport, untrusted-input field-bounding all match the code. OK.

---

## Routing

Nothing to route back. No BLOCK, no WARN. The tree is invariant-clean, drift-free, and docs-synced.

**Leader: cleared to deliver the PR into `develop`** (arm `gh pr merge <n> --auto --squash`; the six required checks are the gate).
