# 04 — Guardian review — WL-040b (faithful ask-thread + ask-group replay on session import)

**Worktree:** `/home/drdave/Desktop/meta/weave-wl040b` · **Branch:** `wl-040b-ask-replay` · **Base:** `origin/develop` @ `dcb36f1`
**Input:** `.handoff/loop/03_verifier_WL-040b.md` = GREEN (all six CI combos; 717 sqlite / 668 libsql / 708 libsql+sign; standing-MCP budget green; lone JobState warning confirmed pre-existing + CI-invisible).
**Verdict: APPROVE.**

---

## Part 1 — Security / correctness invariants

| # | Rule | Status | Evidence |
|---|---|---|---|
| 1 | No shell | **OK** | No `Command::new` / `sh -c` / `format!`-built command in the diff (grep clean). Import spawns no external program. |
| 2 | Parameterized SQL | **OK** | Every new query binds. sqlite: named `params![]` — `import_ask` INSERT (store.rs:4359-4380), `import_ask_group` (store.rs:4412-4416), dedup probes + `list_ask_groups` lookup (store.rs:4264-4288) all `params![...]`, zero interpolated archive data. libsql: positional `params(vec![...])` — `import_ask` 15-col INSERT (store_libsql.rs:3162-3185), `import_ask_group` (3232-3243), `list_ask_groups` (3061-3064), dedup probes (3140-3148, 3219-3222). The only inlined SQL "literals" are `AskState::as_str()` / `AskKind::as_str()` — compile-time `&'static str` from the enums (model.rs:273, 328), never raw archive text. |
| 3 | Untrusted-input bounding BEFORE write | **OK** | `validate_ask_groups` then `validate_asks` run before any write (session.rs:271-272). asker/askee → `check_ident`; subject → `MAX_IMPORT_SUBJECT` (4096); options/close_note/body → `MAX_BODY`; id/reply_to → `ask_id_valid`; parent_id → `ask_many_id_valid`. **state → `AskState::from_str` which is fallible; unknown state REJECTED** (session.rs ~575; model.rs:284-291). Both store seams re-validate (defense-in-depth, store.rs:4322-4357, store_libsql.rs:3101-3122). 5 security tests cover hostile asker / malformed state / oversized options / malformed parent_id / dangling. |
| 4 | No broken links / no forged state | **OK** | A dangling ask (question or claimed-answer message absent from the remap) is `continue`-skipped and counted, never inserted (session.rs:362-376). `import_ask`'s lifecycle bypass is a deliberate materializer over message rows that already exist from the message pass — it inserts no `messages` row and cannot fabricate a thread. Idempotency dedups on the remapped `(asker, askee, question_msg_id)` triple (both backends), correct because `Store::send` returns the existing local id on a dedup hit (impl-log decision #1, verifier check #2). |
| 5 | Layer DAG intact | **OK** | Pure store methods (`import_ask`/`import_ask_group`/`list_ask_groups`) live in `weave-core` (no I/O beyond the DB conn); all file I/O, remap, and HashMap wiring live in `weave/src/session.rs` (bin). No upward dep; no I/O leaked into core. |
| 6 | Destructive ops gated | **OK** | None added. Import is additive/idempotent; `--dry-run` writes nothing (counts would-replay asks excluding danglers, session.rs:430-440). |
| 7 | token-light MCP surface | **OK** | CLI-only. No `tool_catalog()` / standing-tool change. `standing_mcp_surface_is_within_token_budget` green (verifier check #4). |

### WARN (non-blocking)

- **WARN — kind degrades, not rejected (session.rs `AskKind::parse`, model.rs:348).** Gate point 2's phrasing said "state/kind … unknown rejected." That holds for **state** (fallible, rejected) but **not** for **kind**: `AskKind::parse` falls back to `FreeText` on an unknown value. This is **not a security issue** — `kind` only ever reaches SQL as `kind.as_str()` (compile-time `&'static str`), so an unknown value can never be stored raw; the sole effect is a fidelity downgrade of a future/foreign kind to `free_text`, consistent with `AskKind::from_str`'s documented graceful-degradation contract everywhere else in the codebase. The docs do **not** overclaim — CHANGELOG and FORMAT both scope "rejected" to `state` only and document `kind` as defaulting. Acceptable as-is; noting the asymmetry for the record. No change required.
- **WARN — `target_count` imported unbounded (store import_ask_group, both backends).** The group's `target_count: i64` is bound straight from the archive with no upper cap. It is an integer (no injection) and only feeds totality *arithmetic* on read (`failed = target_count - created`, store.rs:4604) — it drives no loop and no allocation, so a hostile large value is at worst a cosmetic display artifact, not a resource hazard. Acceptable; flagging for awareness.

### NOTE (authz consideration — gate point 3, as requested)

`import_ask` materializes `asker`/`askee` directly from the archive (after `--as` remap of the source identity only; third-party names preserved verbatim, exactly like message import). A crafted archive can therefore create local `asks` rows attributed to arbitrary third-party identities. This is **not a new authz surface**: it is the identical trust model as the already-shipped WL-040 message import (a crafted archive can already insert messages from any sender to any recipient). Session import is an operator-chosen, `--as`-scoped LOCAL materialization of an archive, not an authenticated cross-mesh transfer; the asks ride on the messages imported under the same assumption. Documented as designed behavior (ARCHITECTURE.md §"untrusted external input", FORMAT "Security"). No action required.

## Part 2 — Rust-native drift scan

- **OK — No non-Rust intrusion.** Changed files are exclusively `.rs` and `.md`. The only non-source files in the diff are inert `.handoff/loop/*.md` sidecars (backlog + this report's siblings) — metadata nothing builds against.
- **OK — Zero new dependency.** `git diff origin/develop -- Cargo.toml '**/Cargo.toml' Cargo.lock` is **empty**. No new crate, no `tokio`/heavy tree pulled, no feature added. No new build step in any language.
- **OK — Additive envelope, no schema bump, fwd/back compatible.** `ExportedAsk` gained `kind`/`options`/`reply_to`/`close_note`/`parent_id` + new `ExportedAskGroup`/`ask_groups`, all `#[serde(default)]` (session.rs in core). `schema_version` correctly NOT bumped. NEW weave reading OLD export → missing fields default (`kind`→`free_text`, optionals→`None`, `ask_groups`→empty), proven by `older_export_without_new_ask_fields_defaults`. OLD weave reading NEW export → serde ignores the unknown additive fields (no `deny_unknown_fields`). Compat holds both directions.
- **OK — No generated-artifact ↔ code fork, no misinformation drift.** No ECC/agent-config artifact touched; docs match code (see Part 3).

## Part 3 — Docs sync

- **OK — `CHANGELOG.md`** `[Unreleased]` WL-040b block added; accurately scopes "unknown state rejected" (does not overclaim kind).
- **OK — `docs/FORMAT-session-export.md`** full respec: `ask_groups` envelope row, replayed-asks section, per-field `ExportedAsk` table (incl. new fields + `reply_to`-NULL policy), `ask_groups[]` table, dangling/idempotency/dry-run semantics, security paragraph. Matches code field-for-field.
- **OK — `ARCHITECTURE.md`** §session import updated with the 3 new Store methods + out-of-order materializer + group-before-children replay + dangling/reply_to behavior.
- **OK — `docs/REPOWIRE-PARITY.md`** casr "Session export / resume" row → **WL-040 + WL-040b**, ask-thread fidelity complete.
- **OK — `.handoff/loop/backlog.md`** WL-040b → `[x] DONE` with summary.
- **No code↔doc fork detected.**

---

## Overall: APPROVE

All seven invariants hold; SQL fully parameterized in both backends; untrusted archive bounded before write with unknown ask-`state` rejected; dangling refs skipped (never broken-linked); idempotent re-import correct; layering clean; CLI-only (token budget intact); zero new dependency / no non-Rust intrusion; additive envelope is forward/back compatible with no schema bump; docs fully synced with no fork. Two non-blocking WARNs (kind graceful-degradation by design; unbounded-but-inert `target_count`) and one authz NOTE (same trust model as existing message import) are recorded, none warranting a block.

**Cleared for delivery: open the PR into `develop` and arm auto-merge.**
