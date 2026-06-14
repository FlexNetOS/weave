# WL-034 — Static mailbox export — implementer change log

Worktree: `/home/drdave/Desktop/meta/weave-wl034` (branch `wl-034-mailbox-export`).
Implemented per `01_planner_plan.md` + leader decisions (per-identity scope via
`Store::history`; centralize `html_escape` into `weave-core`). No commit / push /
PR performed — leader owns delivery.

## Files touched

| File | Change | Rationale |
|---|---|---|
| `weave-core/src/export.rs` (NEW) | `pub fn html_escape` (moved from dashboard) + `pub fn render_mailbox_html(&[Message]) -> String` + private `script_safe_json` + `#[cfg(test)]` unit tests | Pure render belongs in core (no I/O); single XSS escape source |
| `weave-core/src/lib.rs` | Added `pub mod export;` | Register the new module |
| `weave-mcp/src/dashboard.rs` | Deleted the local `html_escape`; added `use weave_core::export::html_escape;` | DRY — single audited escape helper; no behavior change (the `#[cfg(test)]` XSS regression test now exercises the re-exported fn via `use super::*`) |
| `weave/src/main.rs` | Added `Cmd::Export { out: PathBuf, for_id: Option<String>, limit: Option<usize> }` clap variant + its handler | CLI glue + I/O live in the bin |

No `Store` trait change, no SQL change, no schema change → **the Store/backend
boundary was NOT crossed.** `store_libsql.rs` is untouched (libSQL needs no mirror).
No new dependency (`serde_json` is already a default-build dep of `weave-core`).

## Exact new signatures

```rust
// weave-core/src/export.rs
pub fn html_escape(s: &str) -> String;             // moved verbatim from dashboard.rs
pub fn render_mailbox_html(messages: &[Message]) -> String;
fn script_safe_json(json: &str) -> String;         // private helper
```

```rust
// weave/src/main.rs — new Cmd variant (clap)
Export {
    #[arg(long)]            out: PathBuf,           // required output .html path
    #[arg(long = "for")]    for_id: Option<String>, // identity; --for maps to keyword-safe field for_id
    #[arg(long)]            limit: Option<usize>,   // default 10_000, clamped by store::clamp_limit
},
```

CLI surface: `weave export --out <path> [--for <id>] [--limit N]`.

Handler flow (no shell; argv-only file write):
`resolve_me_explicit(for_id, …)` → `weave_core::store::check_ident("identity", &me)?`
(honors the identity cap + control-char rejection) → `refresh_presence` →
`store.history(&me, None, limit as i64)` (existing read path; `clamp_limit`/`MAX_LIMIT`
bound the limit) → `render_mailbox_html(&rows)` → `std::fs::write(&out, html)?` →
prints `exported N message(s) for '<me>' -> <path>`.

## How the `</script>` neutralization works (the XSS hinge)

The messages are serialized once with `serde_json::to_string(messages)` and embedded
inside a `<script type="application/json" id="weave-data">…</script>` block. That block
is **not** executed and **not** HTML-tag-parsed — but the HTML tokenizer still ends a
`<script>` element at the literal byte sequence `</script` regardless of the `type`
attribute. So before embedding, `script_safe_json` rewrites:

- `</`  → `<\/`   (neutralizes `</script>` — the load-bearing breakout case)
- `<!--` → `<\!--` (defangs the HTML-comment-open "script data" tokenizer state)

`\/` and `\!` are legal JSON string escapes, so the decoded value is **byte-identical**:
the client does `JSON.parse(document.getElementById('weave-data').textContent)` and gets
the original bodies back (a unit test round-trips a `</script><script>alert(1)</script>`
body through the embedded block and asserts equality). A body containing a raw
`</script>` therefore can NOT terminate the data block.

Two further independent barriers:
1. **Static `<noscript>` fallback table** — every `Message` field is interpolated through
   `html_escape` (never raw `format!`), so XSS-safe even with JS disabled.
2. **Client rendering uses `textContent` / `createElement`** (never `innerHTML`) — user
   content is inserted as text nodes, a second barrier independent of the JSON escaping.

No external assets: no `<script src>`, no `<link href>`, no CDN, no `http(s)://` —
double-click-openable offline (asserted by a unit test).

## Build results (run from worktree root)

- `cargo build --release` (default sqlite) — **GREEN** (finished, 71 crates).
- `cargo build --no-default-features --features libsql` — **GREEN** (finished, 213 crates).
- `cargo build --features surfaces` — **GREEN** (dashboard `html_escape` re-use compiles).
- `cargo fmt --all` applied; `cargo fmt --all --check` — **clean**.

Tests were NOT run and NOT written beyond the in-module `#[cfg(test)]` unit tests in
`export.rs` — the integration (`integration.rs`) and security (`security.rs`) layers are
the verifier's job (Phase 3), per instructions.

## Docs sync (guardian BLOCK follow-up — docs only, no code change)

The guardian BLOCKed solely on missing docs sync (all code/security/invariant/drift
axes passed). Added exactly three doc entries to match the final code; no code touched.

| File | Change | Rationale |
|---|---|---|
| `CHANGELOG.md` | `[Unreleased]` → `### Added`: WL-034 `weave export --out <path> [--for <id>] [--limit N]` — self-contained, offline, XSS-safe portable HTML mailbox bundle with client-side search (mcp_agent_mail parity) | User-facing feature belongs in the changelog |
| `README.md` | Added a `weave export` line to the default-build `## CLI` list (after `weave inbox`) — searchable offline HTML of the caller's mailbox (`--for` scopes to another identity, `--limit` caps) | Default-feature CLI command must be documented in the CLI section (not under `--features surfaces`) |
| `ARCHITECTURE.md` | Noted `weave-core/src/export.rs` in the `weave-core` layer tree: pure `render_mailbox_html` + the now-centralized `html_escape` (single XSS-escape source of truth that `weave-mcp` dashboard reuses) | Keep the layer description in sync with the new module |

`cargo fmt --all --check` — **clean** (docs only). `git status` now shows `README.md`,
`ARCHITECTURE.md`, `CHANGELOG.md` modified alongside the WL-034 code/test files.
