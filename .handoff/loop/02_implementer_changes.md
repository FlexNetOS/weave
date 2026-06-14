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

---

# WL-035 + GAP-2 — Mailbox backup/restore + export-write context — implementer change log

Worktree: `/home/drdave/Desktop/meta/weave-batch` (branch `wl-035-037-batch`).
Implemented per `wl035_plan.md` + leader decisions. No commit / push / gh (leader owns delivery).
**Scope held:** WL-035 + the GAP-2 export-write fix only. Did NOT touch the send
path, schema/messages table, or config hook structs (WL-036/WL-037).

## Files touched

| File | Change | Rationale |
|---|---|---|
| `weave-core/src/archive.rs` **(new, pure)** | Hand-rolled uncompressed USTAR tar: `write_archive(&[(&str,&[u8])]) -> Result<Vec<u8>>`, `read_archive(&[u8]) -> Result<Vec<ArchiveEntry>>`, `safe_entry_name(&str) -> Result<()>` traversal guard, `ArchiveEntry`, entry-name constants + `KNOWN_ENTRY_NAMES`. 9 unit tests (round-trip, empty/512-aligned bodies, truncation + checksum rejection, traversal-guard accept/reject). ZERO new deps. | No-dep portable container; pure → unit-testable with no FS. |
| `weave-core/src/lib.rs` | `pub mod archive;` | Expose the module. |
| `weave-core/src/store.rs` | Added `fn snapshot_to(&self, dest: &std::path::Path) -> Result<()>` to the `Store` trait; `SqliteStore` impl issues parameterized `VACUUM INTO ?1` then read-back-verifies (`open_readonly` + `total_messages`). | Consistent snapshot; trait method mirrored in both backends. |
| `weave-core/src/store_libsql.rs` | Mirrored `snapshot_to` on `LibsqlStore` (local `VACUUM INTO ?1` via the `params()` helper + read-back verify); added `local_path: Option<PathBuf>` field (set on local `open`, `None` for remote/read-only); **remote backend bails** with a clear message (no local file to vacuum). | Dual-backend mirror invariant; remote has no local file. |
| `weave/src/backup.rs` **(new)** | `run_backup(cfg, store, out, force)` + `run_restore(cfg, in_path, force)`. Backup: snapshot→verify#1→read config/settings→build archive→atomic rename→**read-back verify#2** (re-parse archive + re-open the embedded DB, assert counts match). Restore: parse→`safe_entry_name` on EVERY entry→stage DB to temp→**verify before touching live store**→clobber guards (`--force`, `.bak` first)→atomic move; **settings.json only with `--force`**; prints "run `weave setup` to re-register the MCP server." All file writes context-wrapped. | One orchestration seam; keeps `main.rs` thin; verify-the-write at both ends. |
| `weave/src/main.rs` | `mod backup;`; `Cmd::Backup { out, force }` + `Cmd::Restore { in_path, force }`; dispatch (Restore in the no-store early block since it replaces the live store; Backup in the main match with the open store); `use anyhow::{Context, Result}`. **GAP-2:** wrapped the `Cmd::Export` final write with `.with_context(\|\| format!("failed to write export to {}", out.display()))?`. | CLI surface + dispatch + the GAP-2 export-write context fix. |
| `weave/src/setup.rs` | `settings_path()` made `pub`. | backup/restore must read/restore the installed `settings.json` hooks. |

## Snapshot + traversal-guard approach

- **Snapshot:** `Store::snapshot_to` uses parameterized `VACUUM INTO ?1` (path BOUND,
  never inlined) — a fully-checkpointed consistent copy, never `fs::copy` of a live
  WAL DB. Both backends read-back-verify the snapshot (re-open read-only + count)
  before returning Ok. Remote libsql has no local file → `bail!` with guidance.
- **Traversal guard:** `safe_entry_name` rejects empty / >100-byte / NUL / absolute /
  any `/`/`\`/`:` separator / `.`/`..`, AND requires the name to be one of the closed
  set `KNOWN_ENTRY_NAMES` (`messages.db`, `config.toml`, `settings.json`, `MANIFEST`).
  `run_restore` runs it on EVERY parsed entry before using any. Read-back verification
  at both ends: backup re-opens the written archive + embedded DB and compares counts;
  restore stages the DB to a temp path and opens+counts it BEFORE replacing the live DB.
- **No shell / argv-only:** entirely in-process Rust + SQLite C calls; no `Command`.
- **Archive contents:** `messages.db` (snapshot) + optional `config.toml` + optional
  `settings.json` + a text `MANIFEST` (version/backend/counts/membership).

## Boundary crossed

**Yes — `Store` trait boundary crossed.** New trait method `snapshot_to` added and
mirrored in BOTH backends (`store.rs` sqlite + `store_libsql.rs` libsql). No schema /
column changes, so no migration needed.

## Build results

- `cargo build --release` (default sqlite) — **green**.
- `cargo build --no-default-features --features libsql` — **green**.
- `cargo clippy --all-targets` (default) — **clean (no issues)**.
- `cargo clippy --no-default-features --features libsql` — **clean (no issues)**.
- `cargo fmt --all` — **applied**.
- `cargo test -p weave-core archive::` — **9 passed** (sanity; full verifier pass runs later).

Tests beyond the archive unit suite were intentionally NOT added (combined verifier
pass owns the integration/security/prop layers). Docs (README/ARCHITECTURE/CHANGELOG)
not yet synced — flagged for the verifier/docs pass per the plan's "Docs to sync".
