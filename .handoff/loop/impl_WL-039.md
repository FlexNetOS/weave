# WL-039 — Idle notification dedup — Implementation change log

Status: implemented; both backends compile + test green. Builds clean on all four
feature combos (sqlite default, libsql, sign, libsql+sign). NOT committed/pushed
(leader owns delivery).
Branch/worktree: `wl-038-042-batch` @ `/home/drdave/Desktop/meta/weave-wl038-042`.
Built on top of WL-038 (`messages.expires_at`, libsql positional index 11) — the new
`kind` column is the NEW trailing projection column (index 12 in libsql positional
`row_to_message`), `expires_at` untouched.

## Build / test verification

- `cargo build` (default sqlite) — clean
- `cargo build --no-default-features --features libsql` — clean
- `cargo build --features sign` — clean
- `cargo build --no-default-features --features "libsql sign"` — clean
- `cargo clippy --all-targets -- -D warnings` (sqlite) — clean
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` — clean
- `cargo fmt --all --check` — clean
- `cargo test` (sqlite black-box) — all green
- `cargo test -p weave-core --features sqlite --lib` — **291 passed** (was 285 pre-change: +6 sqlite store unit)
- `cargo test -p weave-core --no-default-features --features libsql --lib` — **243 passed** (was 239: +4 libsql store unit)
- `cargo test -p weave-mcp --lib` — **24 passed** (+1 catalog)
- `cargo test --no-default-features --features libsql` (full black-box, libsql) — all green

## Store / backend boundary: CROSSED (dual-backend)

Schema + migration + the new `Store::supersede_prior_idle` + every explicit message
projection were mirrored in both `weave-core/src/store.rs` (default sqlite) and
`weave-core/src/store_libsql.rs` (feature-gated libsql). The libsql message
projections are **positional**: `kind` is the trailing index-12 column in EVERY
`SELECT ... FROM messages` that feeds `row_to_message` (8 projections updated), and
`row_to_message` reads index 12. The sqlite `row_to_message`/peek read by name; the
sqlite thread CTE reads `kind` positionally at index 12.

## Marker design (Option A, as locked by leader)

Additive nullable `messages.kind TEXT` column (NULL/`'normal'` == ordinary message),
guarded `column_exists` + `ALTER TABLE ADD COLUMN` migration in both backends (exact
WL-037 pattern). `model::KIND_IDLE = "idle"` const; `Message.kind: Option<String>`
(`#[serde(default)]`). The marker is an internal enum literal, never user free-text.

`kind='idle'` is stamped ONLY inside `supersede_prior_idle` (scoped to `sender` for
authz), which the notify path calls. `Store::send`'s 6-arg shape is unchanged for all
existing callers — least-invasive, as the plan preferred.

## Files touched (rationale each)

### Code
- `weave-core/src/model.rs` — `Message.kind: Option<String>` (+`#[serde(default)]`),
  `pub const KIND_IDLE`. Pure, no I/O.
- `weave-core/src/store.rs` — SCHEMA `messages.kind` (trailing); guarded `ADD COLUMN
  kind` migration; `row_to_message` reads `kind` by name; peek projection + thread CTE
  (index 12) carry `kind`; new `Store::supersede_prior_idle` trait method + SqliteStore
  impl (stamp `kind='idle'` scoped to sender, then auto-supersede prior unread idle
  pings — full predicate the hard safety boundary). + store unit tests.
- `weave-core/src/store_libsql.rs` — item-for-item mirror: SCHEMA, migration list,
  `row_to_message` index-12 read, all 8 message projections gain trailing `kind`,
  `supersede_prior_idle` impl (async/block_on, parameterized). + store unit twins.
- `weave-core/src/export.rs` — `kind: None` on the test-helper `Message`.
- `weave-mcp/src/dashboard.rs` — `kind: None` on the test-helper `Message`.
- `weave-mcp/src/mcp.rs` — `tool_notify` reads `dedupIdle` bool, post-persist calls
  `supersede_prior_idle` (best-effort); `tool_catalog()` adds `dedupIdle` to the
  `weave_notify` op schema (**catalog only — NO new standing tool**; budget test stays
  green). + MCP catalog test.
- `weave/src/main.rs` — `Cmd::Notify` gains `--dedup-idle` flag; post-send
  `supersede_prior_idle` call (mirrors WL-037 `--supersedes` post-stamp ordering;
  best-effort). `Cmd::Send` untouched.

### Tests added (16 total)
- `weave-core/src/store.rs` (sqlite store unit, 7): `supersede_prior_idle_replaces_
  prior_unread_idle`, `idle_dedup_never_touches_real_messages`, `idle_dedup_only_
  supersedes_unread`, `idle_dedup_scoped_to_same_sender_recipient`, `idle_dedup_authz_
  self_only`, `idle_dedup_idempotency_replay_is_noop`, `idle_dedup_kind_column_is_
  migrated_idempotently`.
- `weave-core/src/store_libsql.rs` (libsql store unit, 4): `idle_dedup_replaces_prior_
  unread_idle_libsql`, `idle_dedup_never_touches_real_messages_libsql`, `idle_dedup_
  only_supersedes_unread_libsql`, `idle_dedup_scoped_and_authz_self_only_libsql`.
- `weave-mcp/src/mcp.rs` (1): `catalog_weave_notify_lists_dedup_idle`.
- `weave/tests/integration.rs` (3): `cli_notify_dedup_idle_collapses_to_latest_and_
  spares_real_message`, `cli_notify_without_dedup_idle_keeps_both_unread`,
  `mcp_weave_notify_dedup_idle_collapses_and_spares_real_send`.

The "never dedups a real message" boundary is proven explicitly in both backend store
units (`idle_dedup_never_touches_real_messages*`), the CLI integration test, and the
MCP test (two identical `weave_send` bodies are never deduped).

### Docs (shipped with the code)
- `CHANGELOG.md` — `[Unreleased] / Added` WL-039 bullet (WL-037/WL-038 style).
- `docs/REPOWIRE-PARITY.md` — new "Idle notification dedup (atm-core)" → HAVE row,
  placed next to the WL-037 supersede row.
- `ARCHITECTURE.md` — `messages.kind` column + idle-dedup paragraph (automatic
  supersede on the notify path, reuses the `superseded_by` spine, real-message
  safety boundary), appended to the WL-037/WL-038 schema-column prose.
- `docs/TESTING.md` — WL-039 dual-backend test-layer note after Property 6.
- `README.md` — `weave notify --dedup-idle` usage line.

## Deviations from the plan (with reasoning)

1. **Marker stamping lives inside `supersede_prior_idle`, not in `send`/a wrapper.**
   The plan listed "extend send vs. a dedicated send_idle vs. post-update" as an
   implementer micro-decision (least-invasive, keep `send` 6-arg). I stamp
   `kind='idle'` as the FIRST statement of `supersede_prior_idle` (scoped to `sender`),
   so the notify path's existing post-send call both marks the new ping AND supersedes
   prior ones in one method. This keeps `Store::send` untouched for all callers and
   centralizes every idle-kind write behind the self-only authz. (No invariant impact.)

2. **"Both survive in history" integration assertion uses `weave search`** (not a
   non-existent `weave history` — consistent with WL-038's note). `search` does not
   filter `superseded_by`, so the superseded predecessor is retained and findable;
   `inbox`/`inbox --all` always filter `superseded_by IS NULL` so they cannot show it.

## Invariants upheld

- **Parameterized SQL** — the dedup `UPDATE`, the kind-stamp `UPDATE`, and the
  migration use bound `params!` / `params(vec![...])`; only the additive DDL
  identifier (`kind`) and the broadcast aliases are literals.
- **No-shell** — `dedup_idle`/`dedupIdle` is a bool; `kind` is an internal literal;
  nothing new reaches a spawn.
- **Input caps** — `kind` is never user free-text (an internal enum literal);
  `check_ident` on sender/recipient unchanged; no new unbounded input.
- **MCP stdout discipline** — no new stdout writes in `tool_notify`.
- **Token-light MCP surface (ADR-0003 / WL-051)** — `dedupIdle` added ONLY to the
  `weave_notify` catalog op; no new standing tool; `standing_mcp_surface_is_within_
  token_budget` stays green (asserted).
- **Censorship/DoS guard (WL-037 carry-over)** — `supersede_prior_idle` scopes both
  the kind-stamp and the supersede `UPDATE` to `sender = caller`, so session X can
  never dedup session Y's pings (proven by `idle_dedup_authz_self_only*`).
- **Additive/backward-compatible** — nullable column, `#[serde(default)]`, NULL ==
  legacy ordinary message; old DBs migrate in place (O(1) `ADD COLUMN`), proven
  idempotent on re-open.
- **Layer DAG** — change confined to model (pure) → store (both backends) → mcp/main;
  no upward dep added.
