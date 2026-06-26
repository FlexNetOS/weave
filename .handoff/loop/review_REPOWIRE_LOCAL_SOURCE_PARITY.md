# /review — repowire local source parity audit truth check

Date: 2026-06-26

## Reviewed target

- Completed goal file: `goal/repowire-local-source-parity-audit.md`
- Audit artifact under review: `.handoff/loop/audit_REPOWIRE_LOCAL_SOURCE_PARITY.md`
- Merged PR under review: #155 (`76ca112`, `docs(audit): map local repowire source parity`)
- Local source archive: `/home/drdave/Desktop/meta/meta-yard/repowire-main.zip`
- Zip SHA-256: `b4da7303e7bc06b1ad7a891cb2a90237a2e44194a1ebf1c9edde0caacf943633`

## hf kernel / handoff policy preflight

Before editing this review/correction:

- Fast-forwarded local `develop` to `origin/develop` at `76ca112`.
- Ran `hf status --json`: Weave ledger healthy, `1/2` tasks done, `16` witnessed events verified.
- Ran `hf resume --compact`: next safe task remains `TASK-0001`.
- Ran `hf sync --dry-run`: Weave would roll up `0` events past cursor `16`.
- Ran `hf sync --auto`: Weave rollup succeeded with `appended 0, skipped 0 (past cursor 16)`.
- Ran `hf doctor`: health OK, witness chain OK, cards OK.

Caveat: the `hf sync --auto` KB mirror step reported unrelated `git-kb checkout ... Uncommitted changes exist in workspace` from the broader meta workspace, and some other fleet members still have legacy SQLite ledgers. The Weave local ledger/cards/rollup path was healthy and idempotent.

## Drift guard

Ran the `weave-drift-guard` checks from `.claude/skills/weave-drift-guard/SKILL.md`:

- No foreign toolchain manifests (`package.json`, `pyproject.toml`, `go.mod`, etc.) are tracked in build paths.
- No `.omc`, ECC package, or package directory artifacts are tracked.
- Non-Rust tracked hits are existing inert sidecars or scripts (`.handoff/**`, `.codex/.agents/.claude`, docs, `.github`, `scripts/supply_chain_audit.py`, `scripts/target_smoke.py`) and do not feed the Rust build.
- Verdict: no Rust-native drift introduced by the audit/review docs.

## Triple-check findings

### Finding 1 — review queue row was under-claimed

The audit originally classified `repowire/daemon/routes/reviews.py` as superseded/no direct Weave queue. Current code contradicts that:

- `weave-core/src/store.rs` and `weave-core/src/store_libsql.rs` define the `reviews` table and store methods `add_review_item`, `review_queue`, `mark_reviewed`, `remove_review_item`.
- `weave-mcp/src/mcp.rs` exposes `weave_review_queue`, `weave_review_add`, `weave_review_mark`, `weave_review_remove`.
- `.handoff/loop/backlog.md` marks WL-020 GitHub review queue integration complete.

Correction made: audit row now marks review queue as **covered** by MCP/store parity, not browser-dashboard parity.

### Finding 2 — delivery trace row was under-claimed

The audit originally left `repowire/daemon/routes/traces.py` unclear. Current code proves a native delivery-trace surface:

- `weave-mcp/src/mcp.rs` exposes `weave_delivery` for metadata-only delivery trace lookup.
- `weave-core/src/store.rs` stores/list delivery trace rows and tests that the trace carries no message body.

Correction made: audit row now marks traces as **covered** by MCP/CLI/store parity. It remains intentionally not a browser `/traces/{id}` clone.

### Finding 3 — dashboard parity claim still holds

No contradiction was found for the required dashboard areas in `goal/repowire-local-source-parity-audit.md`:

- peer roster
- selected peer view
- transcript/thread search/reply/pagination
- mesh feed/event stream/reconnect/gap recovery
- pending questions: choice/tool/free-text
- jobs list/detail/result/cancel/retry/recreate
- settings/config surface
- spawn/kill/dialog controls and safety posture
- auth/token/cookie/write gating
- JSON/SSE endpoint compatibility

The two corrections improve non-dashboard parity classification and do not weaken the dashboard conclusion.

## Verification commands

Run for this review/correction:

```bash
cargo fmt --all --check
cargo clippy -p weave --features surfaces --all-targets -- -D warnings
cargo test -p weave-mcp --features surfaces
cargo test -p weave --features surfaces --test integration surfaces_dashboard -- --nocapture
cargo test -p weave-core --features sqlite review_add_list_mark_remove_roundtrip -- --nocapture
cargo test -p weave-core --features sqlite review_rejects_bad_url -- --nocapture
cargo test -p weave-core --features sqlite delivery_log_records_and_lists_oldest_first -- --nocapture
cargo test -p weave-mcp every_catalog_op_is_dispatchable -- --nocapture
cargo test -p weave-mcp catalog_weave_send_lists_ttl -- --nocapture
hf doctor
git diff --check
```

## Verification result

`hf test TASK-0003` passed after tightening the targeted store test filters to compile the sqlite-backed store tests. Final witnessed result: 11 commands green, 53 tests executed. Earlier failed `hf test` attempts were useful fail-closed evidence: they caught zero-test filters for review/trace checks before the final card was marked complete.

## Verdict

The completed goal is now more accurate than the first pass: the local repowire source was mapped, the dashboard-required parity areas remain covered, and post-review corrections prevent two false non-dashboard gaps from persisting in the audit record.
