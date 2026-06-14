# WL-042 — Multi-provider lifecycle hook templates (Codex / Gemini / Aider) — Planner Plan

## Goal

Today `weave setup` only wires **Claude Code** (registers the `weave` MCP server via the
`claude` CLI and idempotently merges four lifecycle hooks — `SessionStart`→`session`,
`UserPromptSubmit`→`prompt`, `Stop`/`SubagentStop`→`wake` — into `~/.claude/settings.json`).
WL-042 (cross_agent_session_resumer / casr parity) generalizes setup so weave can ALSO
scaffold the equivalent lifecycle wiring for other coding-agent providers — **Codex CLI**,
**Gemini CLI**, **Aider** — each writing into that provider's own config file Rust-natively,
using the SAME never-clobber-foreign, idempotent, read-back-verified merge discipline as the
Claude path. The shape is `weave setup --provider <claude|codex|gemini|aider>` (default
`claude`, preserving today's behavior). The written files are **sidecar config** (a provider's
own settings), NOT build/runtime inputs — so this introduces **no language drift** and no new
dependency. The standing MCP surface is unchanged (this is a CLI-only admin capability).

This is a non-trivial, multi-file feature (new provider-template logic + a CLI flag + per-provider
integration tests + docs/parity sync). It is NOT a lite-path change.

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave/src/setup.rs` | bin (setup glue) | Add a `Provider` enum + a per-provider template writer. Generalize the Claude merge primitives (read/parse/atomic-write/`write_private`/backup) so Codex/Gemini/Aider reuse them. Add `run_provider(exe, provider)` dispatch; keep `run(exe)` = `run_provider(exe, Provider::Claude)`. Add read-back verification (verify the written file parses and contains weave's entry before declaring success — overlaps WL-041). Per-provider `merge_*` + `prune_*` + `is_weave_*` matchers + an `uninstall_provider`. | `setup.rs` is the canonical hook-wiring module; this is its generalization. |
| `weave/src/main.rs` | bin (CLI) | Add `--provider <claude\|codex\|gemini\|aider>` (clap `ValueEnum`, default `claude`) to `Cmd::Setup` and `Cmd::Uninstall`. Thread it into `setup::run_provider` / `setup::uninstall_provider`. Keep `--git-hooks` unchanged. Update the `//!` usage doc-comment header. | CLI is where the new flag is parsed; this is the only entry point. |
| `weave/tests/integration.rs` | test (integration) | Add per-provider setup tests (HOME pinned to a temp dir): file created, weave entry present, idempotent re-run (no dup), foreign content preserved, uninstall removes only weave's entry. Plus a `--provider claude` regression test asserting unchanged behavior, and an invalid-`--provider` rejection test. | CLI-flag + host-wiring behavior must be black-box tested against the compiled binary. |
| `README.md` | docs | Extend the "Use with Claude Code" section into a "Use with your coding agent" section documenting `weave setup --provider <…>` and what each provider writes; note any unconfirmed mechanism with a caveat. | User-facing surface. |
| `ARCHITECTURE.md` | docs | Update `§ setup.rs — Claude Code wiring` → "host wiring (multi-provider)": describe the `Provider` enum, the shared merge primitives, the per-provider target file + merge strategy, and the read-back-verify step. | Deep design must reflect the generalization. |
| `docs/MULTI-SURFACE-PARITY.md` | docs | Add a row/note under **Admin (setup / uninstall)** capturing multi-provider host wiring as a casr-parity capability, and flag any provider whose mechanism is scaffolded-with-caveat (so the gap is tracked, not silent — this doc's whole purpose). | Parity matrix is the tracked-gap ledger. |
| `CHANGELOG.md` | docs | `[Unreleased]` entry: `setup`/`cli` — multi-provider lifecycle hook scaffolding (`--provider`). | Required for user-facing changes. |
| `docs/REPOWIRE-PARITY.md` (check) | docs | If it tracks casr / session-resumer parity, add the multi-provider host-wiring line. Implementer to confirm; otherwise skip. | casr is the parity frame for this card. |

## Dual-backend?

**No.** This change is entirely in `weave/src/setup.rs` + `weave/src/main.rs` (the bin crate) and
touches **no** `Store` trait method, no SQL, and nothing in `weave-core`. Neither
`weave-core/src/store.rs` nor `weave-core/src/store_libsql.rs` is touched. (The CI libSQL job
still runs, but no mirrored edit is required.)

## Per-provider design (target file + merge strategy)

The Claude template is the reference: weave wires the SAME four logical lifecycle events to the
SAME four `weave hook <arg>` subcommands (`session`/`prompt`/`wake`), expressed in each provider's
native config format. The command weave invokes is always `<exe> hook <arg>` with the exe
single-quoted (reuse `shell_single_quote`/`hook_command`).

- **Claude (confirmed, today's behavior — the baseline):** target `~/.claude/settings.json`;
  JSON `hooks.{event}[]` merge; MCP registered via `claude mcp add`. UNCHANGED. `--provider claude`
  must reproduce today's output byte-for-byte (regression-tested).

- **Codex CLI (MECHANISM PARTIALLY CONFIRMED — see caveat):** target `~/.codex/config.toml`.
  Codex's documented hook/automation surface is a `notify` program invoked on events (a TOML key
  whose value is an argv array, e.g. `notify = ["<exe>", "hook", "wake"]`). Codex does NOT expose
  the full SessionStart/UserPromptSubmit/Stop granularity Claude does; weave should write the
  `notify` argv array (the closest analogue, mapping to weave's `wake`/drain) and, if Codex's
  config schema supports a richer hook table at implementation time, prefer that. Merge strategy:
  TOML, never-clobber — only set/refresh weave's own `notify` key (or a weave-namespaced table);
  preserve all other keys; idempotent by value-equality on the weave key. **Note:** this repo
  already carries `.codex/*.toml` as an **ecc sidecar** — weave's setup must WRITE Codex config
  **Rust-natively** (no external ecc tool, no shelling to a Codex CLI for config); the ecc sidecar
  is unrelated metadata and must not become a source of truth weave mirrors by hand (drift guard).
  ⚠ **The exact Codex `config.toml` hook/notify schema must be confirmed by the implementer** (see
  Risks). If unconfirmed at implementation time, scaffold the documented `notify` argv form with an
  inline `# weave: <caveat>` comment and a README caveat rather than inventing keys.

- **Gemini CLI (MECHANISM UNCONFIRMED — see caveat):** target `~/.gemini/settings.json` (Gemini
  CLI uses a JSON settings file analogous to Claude's). If Gemini CLI exposes a `hooks`-style block,
  mirror the Claude JSON merge into it. **The exact Gemini settings key for lifecycle hooks is
  UNCONFIRMED.** Implementer must confirm Gemini CLI's hook key (or whether it has one); if it has
  no lifecycle-hook mechanism, scaffold ONLY what it supports (e.g. MCP-server registration in its
  settings, if that is its integration point) and record the limitation in the parity matrix +
  README, rather than writing a key Gemini ignores.

- **Aider (MECHANISM LIMITED/UNCONFIRMED — see caveat):** target `~/.aider.conf.yml` (Aider's
  documented YAML config). Aider's hook surface is limited; it supports config-level options and a
  `--load`/command file rather than rich lifecycle events. Scaffold ONLY what Aider supports — most
  likely a documented config stanza or a startup command that runs `weave hook session`/`prompt` —
  and explicitly mark Aider as **partial** in the parity matrix. **The precise Aider hook
  capability is UNCONFIRMED**; implementer to confirm and scaffold-with-caveat. NOTE: this adds a
  YAML-writing path; YAML serialization must use an existing dep if one is already in the tree —
  **do not add `serde_yaml` or any new dep** for this. If no YAML serializer is already a
  dependency, write the small fixed YAML stanza via hand-rolled string templating with read-back
  parse-verification (the file is tiny and weave-owned), OR defer Aider to a follow-up card rather
  than pulling a new dependency (dependency-light invariant). Implementer to choose and note which.

**Shared discipline (every provider, mirroring the Claude path):**
1. Read existing config; on a NON-NotFound read error, ABORT without writing (the BLOCKER-fix rule
   in `read_settings` — a transient read failure must never truncate a populated file).
2. Merge only weave's own entry; never clobber foreign keys/hooks.
3. Atomic temp+rename write via the existing `write_private` (0o600, secrets-safe) + one-time
   `.weave.bak` snapshot; preserve the original file mode.
4. Idempotent: re-running refreshes a stale weave entry in place, never appends a second.
5. **Read-back verify** (WL-041 overlap): after write, re-read+parse the file and assert weave's
   entry is present and well-formed before printing success; otherwise return an error.
6. `uninstall --provider <p>` removes ONLY weave's entry from that provider's file.

## Invariants in scope

- **No shell, ever** — `setup.rs`/`main.rs`. The written hook *commands* are config values
  consumed by the provider, but weave itself must not `sh -c`; any process weave spawns
  (e.g. an MCP-register CLI for a provider, if applicable) uses `Command::new(bin)` + argv.
  Reuse `shell_single_quote`/`hook_command` so the exe path is one safe word in the written value.
- **stdout discipline** — `setup.rs` prints human status to stdout (it is a CLI command, not the
  MCP server), consistent with the existing Claude path; no JSON-RPC constraint here. (Guardian:
  confirm no setup output leaks onto an MCP path.)
- **Input caps / path safety** — `setup.rs`/`main.rs`. The `--provider` value is a closed clap
  `ValueEnum` (no free text → no injection). Target paths are derived from `HOME` + a fixed
  per-provider suffix (no user-controlled path component).
- **Secrets-safe writes** — `setup.rs`. All per-provider writes go through `write_private`
  (0o600) + atomic rename + one-time `.bak`, because provider settings files can carry API keys.
- **Rust-native / no language drift** — the written provider files are **sidecar config**, not
  build/runtime inputs of weave; no new build step, no new dependency in the default build. The
  Codex ecc sidecar must not become a hand-mirrored source of truth. (Guardian: explicitly verify
  no new dep is pulled — especially for the Aider YAML path — and that nothing weave builds against
  is generated by this.)
- **token-light MCP surface** — UNCHANGED. No new standing MCP tool; this is CLI-only admin. The
  `standing_mcp_surface_is_within_token_budget` test must remain untouched/green.

## Test layers required

All in `weave/tests/integration.rs` (CLI flag + host-wiring → integration is the correct layer;
pure helpers like a new `is_weave_codex_*` matcher SHOULD also get unit tests in `setup.rs`'s
`#[cfg(test)] mod tests`).

**Unit (`setup.rs` `mod tests`):**
- Per-provider `is_weave_*` matcher: matches weave's own entry, rejects look-alikes/foreign
  (mirror the existing `matches_only_real_weave_hooks` style).
- Template-generation purity: given an exe path, the generated entry round-trips through the
  matcher (mirror `hook_command_quotes_and_round_trips`).

**Integration (per provider — Codex, Gemini, Aider; pin `HOME` to a unique temp dir via the
`run_env` / `run_in_cwd_env` helpers, which apply env AFTER `scrub_env` so `("HOME", tmp)` wins):**
1. `setup --provider <p>` creates the provider's config file with weave's entry present.
2. Idempotent re-run: second `setup --provider <p>` reports no-change (or "updated"), no duplicate.
3. Foreign content preserved: pre-seed the provider file with an unrelated key/hook, run setup,
   assert the foreign content is still present afterward.
4. `uninstall --provider <p>` removes ONLY weave's entry, leaving foreign content intact.
5. Read-back verify: a successful setup must leave a parseable file containing weave's entry
   (assert by re-reading the file in the test).

**Regression:**
6. `setup --provider claude` (and bare `setup`, default) produces the existing
   `~/.claude/settings.json` shape — the four hooks — unchanged. (Pin HOME to temp.)
7. Invalid `--provider bogus` is rejected by clap (non-zero exit, usage error) — no file written.

**No new proptest or `tests/security.rs` case is required**: no new security/resource property is
introduced (closed-enum input, fixed paths, existing secrets-safe write path). If the guardian
deems the never-clobber-foreign guarantee a new invariant worth property-testing, add a
`tests/security.rs` property that fuzzes foreign config content and asserts it survives setup;
flag as OPTIONAL.

## Docs to sync

- **CHANGELOG.md** — `[Unreleased]`: `feat(setup): scaffold lifecycle hooks for Codex/Gemini/Aider
  via `weave setup --provider <…>` (casr parity)`; note any provider shipped scaffold-with-caveat.
- **README.md** — generalize the "Use with Claude Code" section to "Use with your coding agent":
  document `weave setup --provider <claude|codex|gemini|aider>`, the target file per provider, and
  an explicit caveat line for any unconfirmed provider mechanism.
- **ARCHITECTURE.md** — update `§ setup.rs — Claude Code wiring` to "host wiring (multi-provider)":
  the `Provider` enum, shared merge primitives, per-provider target+strategy table, read-back-verify.
- **docs/MULTI-SURFACE-PARITY.md** — annotate the **Admin (setup/uninstall)** row with
  multi-provider host wiring; mark partially/unconfirmed providers as ◐ with the tracked caveat.
- **docs/REPOWIRE-PARITY.md** — implementer to check whether it tracks casr/session-resumer; if so,
  add the multi-provider host-wiring line; else skip.

## Edit order

1. **`weave/src/setup.rs`** — add `Provider` enum; factor the generic merge primitives (read,
   atomic-write, backup, read-back-verify) so they take a target path + a format adapter; implement
   `run_provider`/`uninstall_provider` dispatch with Claude routed to the existing path unchanged;
   implement Codex (confirmed-ish), then Gemini, then Aider (scaffold-with-caveat as needed); add
   per-provider `is_weave_*` matchers + unit tests. (Do leaf helpers first, then dispatch.)
2. **`weave/src/main.rs`** — add the `--provider` clap `ValueEnum` to `Setup`/`Uninstall`, thread
   it into `run_provider`/`uninstall_provider`, update the `//!` header. (Depends on step 1's
   public API.)
3. **`weave/tests/integration.rs`** — add the per-provider + regression + invalid-flag tests.
   (Depends on the binary behavior from 1–2.)
4. **Docs** — CHANGELOG, README, ARCHITECTURE, MULTI-SURFACE-PARITY (+ REPOWIRE-PARITY if relevant),
   each carrying the explicit caveats for unconfirmed providers. (Last, reflecting final behavior.)
5. Run the full gate: `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
   `cargo fmt --all --check`, and the libSQL build (no Store change, but CI gates it).

## Risks / open questions

1. **Codex CLI hook schema (PARTIALLY CONFIRMED).** Codex's documented automation surface is the
   `notify` program key in `~/.codex/config.toml`; the exact key name/shape and whether richer
   per-event hooks exist must be confirmed at implementation time. If unconfirmed, scaffold the
   `notify` argv form with an inline caveat comment + README caveat. **Do NOT shell to / depend on
   the ecc tooling** that generates the `.codex/*.toml` sidecar — write Rust-natively.
2. **Gemini CLI hook key (UNCONFIRMED).** Confirm whether Gemini CLI (`~/.gemini/settings.json`)
   has a lifecycle-hook mechanism and its exact key. If it only supports MCP-server registration,
   scaffold that and record the limitation; do not write a key Gemini ignores.
3. **Aider hook capability (UNCONFIRMED / LIMITED).** Confirm what Aider's `~/.aider.conf.yml`
   supports for lifecycle/startup hooks. Aider may support little; scaffold-with-caveat and mark
   **partial**. **YAML dependency hazard:** do NOT add `serde_yaml`/any new dep — write a tiny
   hand-templated YAML stanza with read-back parse-verify, or DEFER Aider to a follow-up card.
   Implementer must choose and document which (dependency-light invariant).
4. **`--provider` on `Uninstall`.** Adding `--provider` to `Cmd::Uninstall` changes its signature.
   Default `claude` preserves today's `weave uninstall` behavior; confirm no other caller of
   `setup::uninstall` exists (it is called once from `main.rs`).
5. **MCP registration per provider.** The Claude path runs `claude mcp add`. Codex/Gemini/Aider may
   or may not have an equivalent MCP-register CLI/config. Where they do, register Rust-natively
   (argv `Command`, no shell); where they don't, scaffold hooks only and note it. Do not assume a
   `claude`-shaped CLI exists for other providers.
6. **Scope boundary.** This card is host-wiring scaffolding only — it must not change the `weave
   hook <arg>` semantics, the MCP surface, or any `Store` behavior. If a provider's mechanism turns
   out to need a NEW `weave hook` variant, that is a separate card; flag it rather than expanding
   scope here.
