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

---

# WL-037 — Message supersede / successor chains — implementer change log

Worktree: `/home/drdave/Desktop/meta/weave-batch` (branch `wl-035-037-batch`).
Implemented per `wl037_plan.md` + leader decisions. No commit / push / gh (leader owns
delivery). **Scope held to WL-037 only** — did not touch archive.rs/backup.rs (WL-035)
or config hook structs (WL-036).

## Schema / migration (additive, both backends)

New nullable `messages.superseded_by INTEGER` (NULL == not superseded). Added to:
- **sqlite** `weave-core/src/store.rs`: SCHEMA `messages` (trailing column after
  `priority`) + guarded `if !column_exists(... "superseded_by") { ALTER TABLE messages
  ADD COLUMN superseded_by INTEGER }` in `migrate()` (the WL-031 priority precedent).
- **libsql** `weave-core/src/store_libsql.rs`: SCHEMA `messages` + a new
  `("messages","superseded_by","ALTER TABLE messages ADD COLUMN superseded_by INTEGER")`
  entry in the `pragma_table_info`-probe migration loop.
- `model.rs`: `Message.superseded_by: Option<i64>` with `#[serde(default)]` (old JSON
  still deserializes). Both `row_to_message` mappers populate it: sqlite **by name**
  (`r.get("superseded_by").unwrap_or(None)`), libsql **by position** at the new index
  **10**.

## Store API — `supersede`

New trait method `fn supersede(&self, caller: &str, old_id: i64, new_id: i64) -> Result<()>`
declared after `set_message_priority`, mirrored in BOTH backends. Behavior:
- Parameterized `SELECT sender FROM messages WHERE id=?1` for `old_id`; bail if it does
  not exist. Parameterized existence probe for `new_id`; bail if missing.
- **Authorization:** bail unless `old_sender == caller` (best-effort same-identity guard;
  censorship/DoS protection — documented as advisory until `sign`).
- `UPDATE messages SET superseded_by=?2 WHERE id=?1` (parameterized). `send` unchanged.

## Projections updated (the libsql positional trap)

EVERY explicit `SELECT id,ts,…,priority` projection feeding a message mapper got
`superseded_by` appended as the trailing (11th) column, in BOTH backends:
- sqlite: `peek_oldest_unread_conn`, the `thread` recursive-CTE projection (read index 10).
  inbox/history/search use `SELECT *` so they pick the column up by name — no projection
  edit needed there.
- libsql (positional, the risk item): `inbox` (both branches), `history` (both branches),
  `search`, `inbox_since`, `peek_oldest_unread_on`, and the `thread` CTE — all extended
  + the mapper reads index 10. Proven aligned by running the libsql inbox/history/thread/
  peek/search/reply/priority tests (see below).

## Read semantics — hide-from-unread, flag-in-history

`AND superseded_by IS NULL` added to the unread/nudge paths in BOTH backends:
- sqlite: `unread_count_conn`, `peek_oldest_unread_conn`, `inbox` (include_read AND
  unread branches), `inbox_since`.
- libsql: `unread_count_on` (covers `unread_count` + `unread_count_tx`),
  `peek_oldest_unread_on`, `inbox` (both branches), `inbox_since`.
- `history`/`thread`/`search` KEEP superseded rows and populate the flag (audit). No-op
  on a legacy store (column NULL everywhere).

## CLI + MCP

- **CLI** `weave/src/main.rs`: `Cmd::Send` gains `--supersedes: Option<i64>`. Local-send
  branch post-stamps `store.supersede(&from, old, mid)` after the priority stamp (rejects
  `old <= 0`); prints `(supersedes #N)`. Cross-store (`--to-store`) branch rejects
  `--supersedes` (no `superseded_by` on `outbox`; refuse rather than silently ignore).
- **MCP** `weave-mcp/src/mcp.rs`: `tool_send` reads optional `supersedes` (rejects `<= 0`),
  post-stamps `store.supersede(&from, old, mid)` after the priority stamp, surfaces a bad
  id as the error string. `tool_catalog()` `weave_send` inputSchema gains a `supersedes`
  integer property. **No new standing tool** — `standing` budget test still passes.

## Test helpers fixed

Two `#[cfg(test)]` `Message { … }` literals needed the new field: `weave-core/src/export.rs`
and `weave-mcp/src/dashboard.rs` (added `superseded_by: None`). The full new WL-037 test
layer (supersede stamps/chain/auth/broadcast, integration, MCP, security, proptest) is the
combined verifier's job per instructions — NOT added here.

## Boundary crossed

**Yes — `Store` trait + schema boundary crossed.** New `supersede` method + new
`messages.superseded_by` column + changed read SQL, all mirrored in BOTH backends; both
`sqlite` (default) and `--features libsql` compile.

## Build / lint / test results (from worktree root)

- `cargo build --release` (default sqlite) — **green**.
- `cargo build --no-default-features --features libsql` — **green**.
- `cargo clippy --lib --bins` (default) — **no issues**.
- `cargo clippy --no-default-features --features libsql -- -D warnings` — **no issues**.
- `cargo fmt --all` applied; `cargo fmt --all --check` — **clean**.
- **libsql positional-mapper proof** (the highest-risk item): `cargo test
  --no-default-features --features libsql` for `inbox` (6), `history` (1), and
  `thread/peek/unread/search/reply/priority` (20) — **all pass**, confirming every
  extended projection lines up with the index-10 read.
- `cargo test -p weave-mcp standing` — **passes** (no standing-token regression).

Docs (CHANGELOG/README/ARCHITECTURE) not yet synced — flagged for the verifier/docs pass
per the plan's "Docs to sync".


---

# WL-036 — Post-send hooks — implementer change log

Worktree: `/home/drdave/Desktop/meta/weave-batch` (branch `wl-035-037-batch`).
Implemented per `wl036_plan.md` + leader decisions. Built ON TOP of the already-committed
WL-035/WL-037 send path (the `--supersedes` post-stamp). No commit / push / PR — leader
owns delivery. No `Store`/schema change; `store_libsql.rs` untouched. No new standing MCP
tool. Docs (CHANGELOG/ARCHITECTURE/README/OPERATIONS/SECURITY/PARITY) NOT yet synced —
flagged for the verifier/docs pass per the plan's "Docs to sync". Full test layer NOT yet
written (combined verifier pass later, per leader).

## Files touched

| File | Change | Rationale |
|---|---|---|
| `weave-core/src/config.rs` | `HookEvent` enum (Send/Ack, total parse, default Send); `PostSendHook` struct (`recipient`/`argv`/`event`/`timeout_ms`, all `#[serde(default)]`) + helpers (`hook_event`, `recipient_pattern` defaulting empty→`*`, `timeout` clamped, `is_valid`); pure `hook_recipient_matches(pattern, recipient)` reusing `model::is_broadcast`; caps `MAX_POST_SEND_HOOKS=16`/`MAX_HOOK_ARGV=64`/`MAX_HOOK_ARG_LEN=4096`/`HOOK_TIMEOUT_MS_DEFAULT=5000`; `post_send_hook: Option<Vec<PostSendHook>>` field (+ Debug line); `Config::hooks_for(event, recipient)` (pure selection, validity drop, `MAX_POST_SEND_HOOKS` bound). | Config schema + pure matcher in the lowest layer (no I/O), reusable by both send paths; broadcast alias single-source via `model::is_broadcast`. |
| `weave-inject/src/inject.rs` | `run_post_send_hook(argv, env, timeout)` — the argv-only/no-shell bounded spawn primitive (resolves `argv[0]` via the existing `resolve_trusted_program`, `.args(&argv[1..])`, message fields via `.envs` only, `try_wait`/kill bounded-wait mirroring `run_bounded_env`); `fire_post_send_hooks(&Config, HookEvent, sender, recipient, subject, message_id)` orchestration (selects via `hooks_for`, builds env, spawns each best-effort, stderr-logs failures); `hook_env` builds the `WEAVE_HOOK_*` vector (+ `WEAVE_HOOK_PAYLOAD` JSON; BODY deliberately NOT exported); `json_escape` tiny helper (no `serde_json` dep added to weave-inject). | The spawn primitive + orchestration sit in `weave-inject` — reachable from BOTH `weave` and `weave-mcp` with no upward dep (single source of truth, plan Option A). |
| `weave-inject/src/lib.rs` | Re-export `fire_post_send_hooks` + `run_post_send_hook`. | Reachable from bin + mcp. |
| `weave/src/main.rs` | Import `HookEvent`; fire `Send` hooks in `Cmd::Send` local (`None`) branch after `inject_and_trace`, in `Cmd::Notify` after the verdict print; fire `Ack` hooks in `Cmd::Ack` after `store.ack` (best-effort `get_ask` for the asker as recipient). | CLI send/notify/ack must fire hooks. |
| `weave-mcp/src/mcp.rs` | Fire `Send` hooks in `tool_send` (BOTH the cross-store-intent early-return per Q3 AND the local-send end before `Ok(out)`) and `tool_notify`; fire `Ack` hooks in `tool_ack` (best-effort `get_ask` asker). Config via `weave_core::config::Config::load()` (the existing in-tool `Config::load()` precedent — avoids plumbing `Config` through `serve`). | MCP send/notify/ack must fire hooks; no new standing tool, no `serve` signature churn. |

## Injection-safe spawn design (the load-bearing invariant)

`run_post_send_hook` is a **no-shell** spawn. `argv` is the FIXED operator-authored Vec from
`config.toml`; weave NEVER substitutes message text into an argv element. `argv[0]` is
resolved to a TRUSTED absolute path via `resolve_trusted_program` (same constraint as a
spawned child program — a hook cannot launch an arbitrary `$PATH` binary; a `None` resolve ⇒
logged to stderr, send unaffected). Remaining elements pass WHOLE via `Command::args`.
Message-derived strings reach the child ONLY as `Command::envs` values
(`WEAVE_HOOK_EVENT`/`SENDER`/`RECIPIENT`/`SUBJECT`/`MESSAGE_ID` + `WEAVE_HOOK_PAYLOAD` JSON);
the BODY is NOT exported (no leak into child env / `ps e`). A hostile subject `"; rm -rf /"`
/ `"$(reboot)"` is an inert env value — no shell exists on this path. Each argv element is
re-validated (`spawn_arg_ok`: len + NUL/control reject) and the count bounded by
`MAX_SPAWN_ARGS` at the spawn primitive; the config layer pre-bounds via `is_valid`. The
wait is bounded (try_wait/kill at `timeout`) so a slow hook never hangs send; every failure
(missing trusted binary / non-zero exit / timeout / spawn error) is caught and logged to
STDERR only (`eprintln!` in inject; never the JSON-RPC stdout frame), NEVER propagated.

## Fire call-sites instrumented (single shared helper `inject::fire_post_send_hooks`)

1. CLI `Cmd::Send` local `None` branch — `Send`, after `inject_and_trace`.
2. CLI `Cmd::Send` cross-store `Some(store_path)` branch — N/A in CLI (the supersede/intent
   guard rejects early; the MCP cross-store intent DOES fire per Q3, see 6).
3. CLI `Cmd::Notify` — `Send`, after the verdict print.
4. CLI `Cmd::Ack` — `Ack`, after `store.ack` (recipient = `get_ask().asker`, best-effort).
5. MCP `tool_send` local end — `Send`, before `Ok(out)`.
6. MCP `tool_send` cross-store intent early-return — `Send` (Q3: a queued intent IS a send).
7. MCP `tool_notify` — `Send`, before the return.
8. MCP `tool_ack` — `Ack` (recipient = `get_ask().asker`, best-effort).

Decisions applied (no re-litigation): Q1 empty/missing `recipient` ⇒ `*` (match all).
Q2 whole-string `*` only (no glob crate, no substring matcher). Q3 cross-store intent fires
`Send`. Q4 BODY not exported. Q5 bounded-synchronous spawn. Q7 broadcast fires once per send
(fire point is the send call-site). Ack hooks use subject-slot = correlation id, message_id = 0.

## Config schema

```toml
[[post_send_hook]]
recipient  = "agent-a"        # "*" = any; a BROADCAST alias matches a broadcast; else exact. omit/empty ⇒ "*"
argv       = ["/usr/bin/tee", "/tmp/sentinel"]   # argv[0] resolved to a TRUSTED abs path; no shell
event      = "send"           # "send" (default) | "ack"
timeout_ms = 5000             # clamped [MIN_TIMEOUT_MS=50, MAX_TIMEOUT_MS=600000]; omit ⇒ 5000
```
File-only (DELIBERATELY no env overlay — a hook is a program to spawn; env injection of one
would be unsafe). Set capped at `MAX_POST_SEND_HOOKS=16`; invalid/oversized rules dropped at
selection with a one-line stderr note.

## Build results (from worktree root)

- `cargo build --release` (default sqlite) — GREEN.
- `cargo build --no-default-features --features libsql` — GREEN.
- `cargo build --features surfaces` — GREEN.
- `cargo clippy --all-targets` — no issues.
- `cargo fmt --all` applied; `cargo fmt --all --check` — clean.

(The `unused import: JobState` warning in `weave-core/src/store.rs` is PRE-EXISTING and
unrelated to WL-036.)
