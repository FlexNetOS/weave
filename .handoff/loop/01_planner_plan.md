# WL-034 — Static mailbox export (self-contained portable HTML bundle with search)

_Plan written by weave-planner. Worktree: `/home/drdave/Desktop/meta/weave-wl034` (branch `wl-034-mailbox-export`, base origin/develop @ 7368417). Planning only — no delivery._

## Goal

Add a default-build `weave` CLI subcommand that writes **one** self-contained `.html`
file containing the mailbox messages, with **client-side** search/filter (vanilla JS over
an inlined JSON blob) — no external assets, no CDN, openable offline by double-click. This
is mcp_agent_mail parity. The feature must stay dependency-light (no new crates/deps), live
in the **default** build (NOT behind `surfaces`/`obscura`), respect the layer DAG (pure
render in `weave-core`, I/O in the `weave` bin), add **no** standing MCP tool, and add **no**
schema or `Store` method change.

## Key decisions (resolve up front)

1. **html_escape: centralize into `weave-core` (option a — preferred).** The existing
   `html_escape` lives in `weave-mcp/src/dashboard.rs` line 23, and that whole module is
   gated `#[cfg(feature = "surfaces")]` (`weave-mcp/src/lib.rs:2`) — so it is **not**
   reachable from the default-build export path. Move the pure `html_escape` fn into
   `weave-core` (new `weave-core/src/export.rs`, re-exported) as the single source, and
   have `weave-mcp/src/dashboard.rs` **re-use** `weave_core::export::html_escape` instead of
   defining its own. DRY, single escape helper, and the dashboard's existing XSS regression
   test keeps guarding it. (If the verifier finds the dashboard re-use churns too much, the
   fallback is a `pub(crate)`/local copy in `export.rs`, but the centralize path is the
   directive and is clean.)

2. **Export scope = per-identity, reusing `Store::history` — ZERO Store/schema change
   (preferred).** There is **no** existing `Store` method that returns *all* messages
   regardless of identity (confirmed: `history(me, peer, limit)` at `store.rs:89/3000` is
   scoped to `me` as sender/recipient/broadcast; `search` is FTS-scoped; `inbox*` is
   recipient-scoped). The token-light, no-dual-backend design is therefore **`weave export
   --for <id>`** (required), which calls `store.history(&id, None, limit)` — the exact same
   read path the existing CLI history block uses (`main.rs:4497`). This keeps WL-034 to a
   pure-render + CLI-glue change with **no** `Store` trait change and **no** `store_libsql.rs`
   mirror. **Do NOT** add an `all_messages()` Store method for v1 — an unscoped "whole DB"
   export is a separate, dual-backend change; if the loop leader wants it, flag it as a
   follow-up (WL-034b) rather than smuggling a Store change into this item. (See Risks.)

3. **MCP: CLI-only (no standing tool, no catalog op) for v1 — recommended.** Export writes a
   file to a server-side path; it is an operator/offline action, not an agent message op.
   Exposing it via MCP would add catalog surface for little agent value. Keep it CLI-only.
   The token-light invariant is satisfied trivially (zero MCP delta).

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/export.rs` (NEW) | model/core (no I/O) | `html_escape` (moved here) + `render_mailbox_html(&[Message]) -> String` pure fn + `#[cfg(test)]` unit tests | Pure render belongs in core; single escape source |
| `weave-core/src/lib.rs` | core | `pub mod export;` | Register the new module |
| `weave-mcp/src/dashboard.rs` | mcp (surfaces) | Replace local `html_escape` with `use weave_core::export::html_escape;` (re-use); keep the XSS test | DRY single source; no behavior change |
| `weave/src/main.rs` | main (bin) | Add `Cmd::Export { out, r#for, limit, … }` variant + handler: resolve id, `store.history`, `render_mailbox_html`, write file | CLI glue + I/O lives in the bin |
| `README.md` | docs | Add `weave export` to the CLI list + a short feature blurb | User-facing surface |
| `ARCHITECTURE.md` | docs | Note the export render fn in the core layer / "where things live" | Keep arch doc synced |
| `CHANGELOG.md` | docs | `[Unreleased] → Added` entry (WL-034) | User-facing change |
| `weave/tests/integration.rs` | test | `weave export` writes a valid self-contained file | CLI subcommand test (TESTING §8.2) |
| `weave/tests/security.rs` | test | XSS / `</script>` neutralization property on the rendered bundle | Security property (TESTING §8.6) |

## Function signatures

```rust
// weave-core/src/export.rs
/// The single XSS escape helper (moved from dashboard.rs). Escapes & < > " ' .
pub fn html_escape(s: &str) -> String;

/// Pure: render a self-contained HTML mailbox bundle from messages.
/// - No I/O, no Store, no socket (unit-testable with a Vec<Message>).
/// - Inlines the messages as JSON in a <script type="application/json"> block,
///   with the JSON SAFELY embedded (see "Self-contained HTML approach").
/// - Embeds the vanilla-JS search/filter inline (no external <script src>/<link>).
/// - Every Message-derived string rendered as static HTML goes through html_escape.
pub fn render_mailbox_html(messages: &[Message]) -> String;
```

```rust
// weave/src/main.rs — new subcommand variant
/// Export the mailbox to a self-contained, offline-openable HTML file with
/// client-side search.
Export {
    /// identity whose mailbox to export (sender/recipient/broadcast scope).
    /// Falls back to resolve_me() like other subcommands when omitted.
    #[arg(long = "for")]
    r#for: Option<String>,
    /// output path for the .html bundle.
    #[arg(long)]
    out: String,
    /// max messages to include (clamped to MAX_LIMIT in the store).
    #[arg(long, default_value_t = 10_000)]
    limit: i64,
},
```

Handler sketch (in the existing `match cmd { … }`):
```rust
Cmd::Export { r#for, out, limit } => {
    let (me, explicit) = resolve_me_explicit(r#for, None, &cfg);
    let _ = explicit;
    let rows = store.history(&me, None, limit)?;          // existing read path
    let html = weave_core::export::render_mailbox_html(&rows);
    std::fs::write(&out, html)?;                          // argv-only; no shell
    println!("exported {} messages -> {out}", rows.len());
}
```

## CLI surface

`weave export --out <path> [--for <id>] [--limit N]`

- `--out <path>` (required): destination `.html` file.
- `--for <id>` (optional): identity whose mailbox to export; defaults via `resolve_me`
  (explicit flag > `$WEAVE_SESSION` > basename(cwd)) — same resolution as `inbox`/`history`.
- `--limit N` (default 10_000): forwarded to `store.history`, where `clamp_limit`
  (`store.rs:1283`, `MAX_LIMIT = 10_000`) bounds it. No new cap constant needed.
- No `--json` (output is a file, not a stream). Prints a one-line summary to stdout.

## Self-contained HTML approach (and why it is XSS-safe)

- **One file, no external refs.** The template embeds the JS in an inline `<script>…</script>`
  and any CSS in an inline `<style>…</style>`. No `<script src>`, no `<link href>`, no CDN —
  double-click-openable offline.
- **Messages embedded as JSON in `<script type="application/json" id="weave-data">`.** The
  client JS reads `JSON.parse(document.getElementById('weave-data').textContent)`. A
  `type="application/json"` block is NOT executed and NOT HTML-parsed for tags **except** that
  the HTML tokenizer still ends the block at the literal byte sequence `</script`. So the JSON
  is serialized with `serde_json::to_string(&messages)` and then made script-safe by
  escaping the three sequences that can break out of / confuse a script element:
  - `</`  → `<\/`  (neutralizes `</script>`, the load-bearing case — a body containing
    `</script>` must NOT terminate the data block),
  - `U+2028`/`U+2029` → ` `/` ` (defensive; line separators),
  - and optionally `<!--`/`<script` per the HTML "script data" escaping rules.
  `<\/` is valid JSON (`\/` is a legal escape) so `JSON.parse` still succeeds. This is the
  classic "safe JSON-in-script" embedding. **No raw `</script>` can survive.**
- **Static HTML cells go through `html_escape`.** The non-JS fallback rows (a `<noscript>`/
  server-rendered table of sender/recipient/subject/body/ts) interpolate every Message field
  via `html_escape` — never `format!("…{body}…")` of raw text. So even with JS disabled there
  is no XSS, and the escape helper is the single audited source (decision 1).
- **Client search = vanilla JS, no deps.** A text `<input>` filters the parsed array in JS by
  substring match over sender/recipient/subject/body, re-rendering rows with
  `textContent`/`createElement` (NOT `innerHTML` of message text) so user content is inserted
  as text nodes — a second XSS barrier independent of the JSON escaping.

## Which Store method supplies the messages

`Store::history(me: &str, peer: Option<&str>, limit: i64) -> Result<Vec<Message>>`
(trait `weave-core/src/store.rs:89`; sqlite impl `:3000`; already mirrored in
`store_libsql.rs`). Called as `history(&me, None, limit)` — sender OR recipient OR broadcast
for `me`, newest-capped then reversed to chronological. **No schema change, no new Store
method, no `store_libsql.rs` edit.**

## Dual-backend?

**No.** WL-034 adds no `Store` trait method, no SQL, and no schema column. It reuses the
existing `history` query that is already implemented + mirrored in both backends. Nothing to
mirror in `store_libsql.rs`. (If decision 2 is overridden to add an unscoped `all_messages()`,
THEN it becomes dual-backend — see Risks; that is explicitly out of scope for v1.)

## Invariants in scope

- **No shell, argv-only** — `weave/src/main.rs` export handler writes the file with
  `std::fs::write` (no `Command`, no shell); message text never reaches a process arg.
- **Parameterized SQL** — inherited; `history` already uses `params!` + the compile-time
  `BROADCAST_SQL`. No new SQL.
- **XSS / output safety (the dominant risk here)** — `weave-core/src/export.rs`: every
  Message-derived static-HTML interpolation goes through `html_escape`; the inlined JSON is
  `</script>`-neutralized (`</` → `<\/`); client JS uses `textContent`, not `innerHTML`.
- **Input caps** — `--limit` is clamped by `clamp_limit`/`MAX_LIMIT` in the store; body length
  is already capped at write time (`MAX_BODY`). No new uncapped input.
- **Default build dependency-light** — no new crate/dep; `serde_json` is already a dep; the
  render fn is std-only string building.
- **stdout discipline** — export is a CLI command (not the MCP server), so its one-line
  summary to stdout is fine; it does not run inside the JSON-RPC stdout path.
- **Token-light MCP surface (ADR-0003)** — zero MCP delta (CLI-only), so the standing
  `tools/list` byte budget is untouched.

## Test layers required (TESTING.md §8)

1. **Pure unit tests — `weave-core/src/export.rs` `#[cfg(test)]`** (§8.1):
   - `render_is_self_contained`: rendered string contains NO `<script src` and NO
     `<link ` external refs; contains an inline `<script type="application/json"`.
   - `render_escapes_static_fields`: a Message with body `<img src=x onerror=alert(1)>` and
     sender `<b>` → the static HTML region contains `&lt;img` / `&lt;b&gt;`, not the raw tag.
   - `render_neutralizes_script_close_in_json`: a Message body containing the literal
     `</script><script>alert(1)</script>` → the rendered output contains **no** raw
     `</script>` that could close the data block (assert `</` is escaped to `<\/` inside the
     data block); and `JSON.parse`-shape sanity (the JSON region still parses — assert via
     `serde_json::from_str` on the extracted block round-tripping the messages).
   - `render_empty_mailbox`: `render_mailbox_html(&[])` produces a valid, non-panicking
     bundle with an explicit "no messages" state.
   - `html_escape_basics`: the moved helper still escapes `& < > " '` (move-parity).

2. **Integration test — `weave/tests/integration.rs`** (§8.2): using `common` helpers,
   `weave send` a couple of messages, then `weave export --for <id> --out <tmp>.html`; assert
   exit 0, the file exists, it is self-contained (no `http://`/`https://` asset URLs, no
   `<script src`), and it contains the message bodies (escaped form). Use the test `WEAVE_DB`
   + a temp out-path under the test's temp dir.

3. **Security property — `weave/tests/security.rs`** (§8.6): `weave send` a body containing
   `</script><script>alert('xss')</script>` and a `<img onerror>` payload, export, then assert
   the rendered file contains **no** un-neutralized `</script>` inside the data block and no raw
   `<img ... onerror` in the static region (verbatim-hostile-input handling → safe embedding).

4. **No new MCP test** — CLI-only; nothing to add to the `McpServer` suite (and the standing
   budget test is unaffected).

5. **Both backends** — no Store change, so the libSQL column needs no new test; the existing
   `--no-default-features --features libsql` run must still pass (the new tests are
   backend-agnostic: they drive the compiled binary).

## Docs to sync (same PR)

- **README.md** — add `weave export --out <f>.html [--for <id>] [--limit N]` to the `## CLI`
  list (~line 50) with a one-line "self-contained, offline, client-side search" blurb. (NOT
  under the `## Human surfaces (--features surfaces)` section — this is default-build.)
- **ARCHITECTURE.md** — add `export.rs` to the `weave-core` layer description / "where things
  live", and note that `html_escape` is now centralized in `weave-core` and re-used by the
  dashboard.
- **CHANGELOG.md** — `[Unreleased] → ### Added`: a WL-034 entry describing the default-build
  `weave export` self-contained HTML bundle.
- **CONTRIBUTING.md** — no change expected (no new invariant/rule).

## Edit order (dependency-respecting)

1. Create `weave-core/src/export.rs` with `html_escape` (moved) + `render_mailbox_html` +
   unit tests; add `pub mod export;` to `weave-core/src/lib.rs`.
2. Update `weave-mcp/src/dashboard.rs` to `use weave_core::export::html_escape;` and delete
   its local copy (keep the XSS regression test pointing at the re-exported fn).
3. Add `Cmd::Export { … }` + handler to `weave/src/main.rs`.
4. Add the integration test (`integration.rs`) and the security test (`security.rs`).
5. Sync README / ARCHITECTURE / CHANGELOG.
6. Run the full gate: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test --all-targets`, plus the libSQL column
   (`cargo clippy/build/test --no-default-features --features libsql`) and the `surfaces`
   build (so the dashboard `html_escape` re-use compiles:
   `cargo clippy --features surfaces -- -D warnings`).

## Risks / open questions

- **Unscoped "whole DB" export.** v1 is per-identity (`--for`, via `history`). A true
  all-senders mailbox dump would need a new `all_messages(limit)` Store method mirrored in
  BOTH `store.rs` and `store_libsql.rs` (dual-backend, schema-free but trait-changing). This is
  deliberately deferred (call it WL-034b). **Open question for the leader:** does mcp_agent_mail
  parity require the *entire* DB, or is per-identity acceptable for v1? Recommend per-identity
  v1; escalate only if parity demands the full dump.
- **Huge mailbox.** `MAX_LIMIT = 10_000` bounds row count; the inlined JSON could still be
  multi-MB. Acceptable for an offline artifact; document `--limit` as the throttle. No
  streaming needed for v1.
- **`</script>` in bodies** — the single load-bearing XSS case; handled by `</`→`<\/`
  escaping of the serialized JSON before it lands in the `application/json` block, plus
  `textContent` rendering client-side. The security test pins this.
- **`html_escape` move churn** — moving it to `weave-core` touches the `surfaces`-gated
  dashboard; the `surfaces` build must be compiled in the gate (step 6) or the move could
  pass the default build but break `--features surfaces` in CI. Flagged for the verifier.
- **`r#for` / `--for`** — `for` is a Rust keyword; use `#[arg(long = "for")] r#for: Option<String>`
  (or rename the field to `whose` with `#[arg(long = "for")]`). Implementer's choice; the CLI
  flag must read `--for`.
