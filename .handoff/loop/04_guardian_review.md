# WL-034 — Static mailbox export — guardian review

Reviewer: weave-guardian. Worktree `/home/drdave/Desktop/meta/weave-wl034`
(branch `wl-034-mailbox-export`, base origin/develop). Change is UNCOMMITTED.
Verifier report: GREEN (sqlite 590 passed; libsql 550 passed/1 pre-existing ignore;
surfaces + sign clippy clean). Review only — no commit/push/PR.

## Verdict: APPROVE

Prior BLOCK was a single docs-sync gap. The implementer has now added all three
required doc entries (truthful, accurate, default-build placement) and additionally
corrected a stale `ARCHITECTURE.md` reference. Code/security/invariant/drift/test
axes were already clean and the code is byte-unchanged since the prior review.
All gates green; `cargo fmt --all --check` clean (exit 0).

---

## Part 1 — Security/correctness invariants  (unchanged — OK)

All eight invariants pass exactly as in the prior review (code unchanged):
no shell (`std::fs::write`, pure `render_mailbox_html`), no new SQL (reuses bound
`Store::history`), layer DAG intact (`export.rs` in `weave-core`, imports only
`crate::model`; dashboard takes a *downward* dep `use weave_core::export::html_escape`),
single centralized `html_escape` (no behavior fork), input caps + id validation
enforced (`check_ident` + clamped `--limit`), read-only (no destructive path),
zero MCP delta (CLI-only, token-light budget untouched). XSS escaping confirmed
sufficient (JSON `script_safe_json` neutralizes `</`/`<!--`; `<noscript>` table
`html_escape`d; client renders via `textContent`/`createElement`). No regressions.

## Part 2 — Rust-native drift scan — OK

Re-run on the full diff: every changed/new file is `.rs` or `.md`. **No Cargo.toml
change anywhere** (no new dependency; `serde_json` pre-existing). No `build.rs`/CI/
`.codex`/`.agents`/`.omc` intrusion. No `Store`/schema change → `store_libsql.rs`
untouched, libsql build/test green. No misinformation drift — docs now match code
(verified below). Rust-native and dependency-clean.

## Part 3 — Docs sync — **OK (was BLOCK, now resolved)**

`git -C … diff -- CHANGELOG.md README.md ARCHITECTURE.md` verified against code:

- **OK — `CHANGELOG.md`**: `[Unreleased] → ### Added` now carries a truthful WL-034
  `weave export --out <path> [--for <id>] [--limit N]` entry — describes the
  self-contained, offline, XSS-safe portable HTML with client-side search, the
  `script type="application/json"` `</`/`<!--` neutralization, `textContent`/
  `createElement` rendering, and locates `render_mailbox_html` in
  `weave-core/src/export.rs` as the centralized `html_escape` owner. Matches code.
- **OK — `README.md`**: `weave export --out mailbox.html …` added to the CLI list in
  the **default-build** section (line ~61, alongside `sessions`), **not** under
  `--features surfaces`. Confirmed correct: the `Cmd::Export` variant in
  `weave/src/main.rs:842` carries **no** `#[cfg(feature = "surfaces")]` (unlike the
  adjacent `Slack` variant at :837) — it is genuinely a default-build command.
- **OK — `ARCHITECTURE.md`**: now lists
  `weave-core/src/export.rs  pure render_mailbox_html + the centralized html_escape`
  in the crate layout, and the XSS section reference was corrected
  `dashboard::html_escape` → `weave_core::export::html_escape` ("reused by the
  dashboard"). Repo-wide grep for `dashboard::html_escape` (`.rs`+`.md`): **NONE
  remaining**. `weave-core/src/lib.rs:3` exports `pub mod export;` and
  `weave-mcp/src/dashboard.rs:17` does `use weave_core::export::html_escape;` — docs
  describe the real code, no fork.

CONTRIBUTING.md: OK (no new invariant/rule).

## Part 4 — Test-layer adequacy — OK (unchanged)

Pure unit (`export.rs`), integration (3 `export_*` tests), security
(`export_neutralizes_script_breakout_and_event_handler`) all present and green on
both backends. Adequate.

---

## Overall: APPROVE

All three axes (Invariants, Drift, Docs) clear. Docs now accurately describe the
shipped code with no fork; the prior stale `dashboard::html_escape` reference is
eliminated. Verifier GREEN, fmt clean. The leader may proceed to commit/handoff.
