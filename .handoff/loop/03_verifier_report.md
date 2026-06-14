# WL-034 — Static mailbox export — verifier report

Worktree: `/home/drdave/Desktop/meta/weave-wl034` (branch `wl-034-mailbox-export`).
Verifier added the integration + security test layers, fixed one defective unit-test
assertion (see "Bug found"), and ran the full dual-backend gate. **No production code
was changed.** No commit / push / PR — leader owns delivery.

## Overall verdict: GREEN

All gate commands pass on both backends; the `surfaces` and `sign` feature builds are
clippy-clean. The export feature is correctly self-contained and XSS-safe.

## Test layers added (per docs/TESTING.md §8)

| Layer | File | Case(s) |
|---|---|---|
| Unit (already present, 1 fixed) | `weave-core/src/export.rs` `#[cfg(test)]` | `html_escape_basics`, `render_is_self_contained`, `render_escapes_static_fields` (**fixed**), `render_neutralizes_script_close_in_json`, `render_empty_mailbox` |
| Integration (NEW) | `weave/tests/integration.rs` | `export_writes_self_contained_html_with_message_text`, `export_for_scopes_to_one_identity`, `export_limit_caps_message_count` |
| Security (NEW) | `weave/tests/security.rs` | `export_neutralizes_script_breakout_and_event_handler` |

Integration coverage: exit 0, file exists, `<!doctype html>` + inline `<style>`, NO
`<script src>` / `<link >` / `http(s)://`, message bodies present (escaped), `--for`
per-identity scoping (no cross-identity leak), `--limit N` caps + keeps the newest rows.
Security coverage: hostile `</script><script>…</script>` body does not survive verbatim
and its inner `<script>…</script>` never appears as live markup (neutralized to `<\/script>`
in the JSON block / `&lt;…&gt;` in noscript); `<img … onerror=…>` is html_escape'd in the
`<noscript>` region.

## Bug found (test-code defect — fixed by verifier; production code is correct)

`weave-core/src/export.rs::render_escapes_static_fields` (the implementer's own unit test)
asserted `!html.contains("<img src=x onerror=alert(1)>")` against the **whole document**.
That FAILED:

```
thread 'export::tests::render_escapes_static_fields' panicked at weave-core/src/export.rs:207:9:
raw payload must not survive in static HTML
test result: FAILED. 250 passed; 1 failed; ...
```

Root cause: `serde_json` does not escape `<`/`>`, and `script_safe_json` only rewrites
`</` and `<!--`. A `<img …>` body has no `</`, so it legitimately appears **raw inside the
inert `<script type="application/json">` data block** — which is correct and safe (that
block is not HTML-parsed; it is read via `textContent`; only a literal `</script` could
terminate it, and `</` is already neutralized). The production `render_mailbox_html` is
sound; only the test's assertion scope was wrong (it should have asserted on the static
`<noscript>` region, which is what its own comment claimed). Fix: scoped the "no raw tag"
assertion to the extracted `<noscript>…</noscript>` region. This is a test-only change;
no implementer routing needed. (The same overbroad-assertion trap was avoided in the new
security test, which scopes the `<img>` check to the noscript region for the identical reason.)

## Full gate results (run from worktree root)

| Command | Exit | Result |
|---|---|---|
| `cargo fmt --all --check` | 0 | GREEN |
| `cargo clippy --all-targets -- -D warnings` (sqlite) | 0 | GREEN — no issues |
| `cargo test --all-targets` (sqlite) | 0 | GREEN — **590 passed**, 0 failed (7 suites) |
| `cargo clippy --no-default-features --features libsql -- -D warnings` | 0 | GREEN — no issues |
| `cargo test --no-default-features --features libsql` | 0 | GREEN — **550 passed, 1 ignored** (10 suites) |
| `cargo clippy --features surfaces -- -D warnings` | 0 | GREEN — no issues (dashboard `html_escape` re-use compiles) |
| `cargo clippy --features sign -- -D warnings` | 0 | GREEN — no issues |

The 1 ignored libSQL test is the pre-existing env-gated live-Turso pull
(`integration.rs:5691`, `#[ignore]` — unrelated to WL-034). No `#[ignore]` was added.

## Cross-boundary checks

- **Store/backend boundary NOT crossed.** WL-034 adds no `Store` trait method, no SQL, no
  schema column — it reuses the existing `history(me, None, limit)` read path. `store_libsql.rs`
  is untouched. Verified the libSQL backend builds + tests green regardless (backend-agnostic
  black-box tests). No drift between the two `Store` impls to reconcile.
- **`html_escape` centralization.** `weave-mcp/src/dashboard.rs` now `use weave_core::export::html_escape;`
  (local copy deleted). The dashboard's XSS regression tests (`html_escape_round_trips_significant_chars`,
  `xss_payload_is_escaped_in_rendered_page`) pass against the centralized helper under
  `--features surfaces` — single audited escape source, no behavior fork.
- **JSON-in-`<script>` ↔ client parse round-trip.** `render_neutralizes_script_close_in_json`
  extracts the embedded `application/json` blob, confirms no raw `</` survives, and round-trips
  it through `serde_json::from_str` back to the original `</script>…`-containing body — the
  embed is byte-identical after `JSON.parse`, so the neutralization is lossless.
- **token-light MCP surface:** zero MCP delta (CLI-only export) → standing `tools/list` byte
  budget untouched; the budget test is unaffected.

## Files changed by the verifier (test code only)

- `weave/tests/integration.rs` — added 3 export integration tests (default build).
- `weave/tests/security.rs` — added 1 export security test (default build).
- `weave-core/src/export.rs` — fixed `render_escapes_static_fields` assertion scope (test code).
