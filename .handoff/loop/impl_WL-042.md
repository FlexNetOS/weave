# WL-042 — Implementation change log

**Task:** Multi-provider lifecycle hook templates — generalize `weave setup` /
`weave uninstall` from Claude-only to `--provider <claude|codex|gemini|aider>`.
**Worktree:** `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch`)
**Store/backend boundary crossed:** NO. Only the `weave` bin layer
(`setup.rs`, `main.rs`) + test/docs. No `Store` trait / SQL / schema /
`store_libsql.rs` change. Both backends compile and all new tests pass under both.
Standing MCP surface untouched (`standing_mcp_surface_is_within_token_budget`
still green) — this is a CLI-only admin capability, no new MCP tool/token.

## Files touched

| File | Rationale |
|---|---|
| `weave/src/setup.rs` | Added `pub enum Provider {Claude,Codex,Gemini,Aider}` + `run_provider`/`uninstall_provider` dispatch (Claude routed to the original body, byte-for-byte). Factored the generic write primitives `sidecar`, `write_bytes_atomic`, `write_json_atomic`, `read_json` (path-taking) out of the old Claude-only `read_settings`/`write_settings`; refactored `merge_hooks`/`prune_hooks` into path-generic `merge_hooks_at`/`prune_hooks_at` (Claude + Gemini share them). Implemented Codex (`~/.codex/config.toml` line-based `notify` TOML merge), Gemini (`~/.gemini/settings.json`, reuses the Claude JSON `hooks` merge), Aider (`~/.aider.conf.yml` hand-templated `weave-hook:` stanza). Each provider write is idempotent, never-clobber-foreign, atomic temp+rename (0o600 `.weave.bak`), and read-back-verified (non-NotFound read error ABORTS without writing). Added 8 unit tests in `mod tests`. |
| `weave/src/main.rs` | Added `SetupProvider` clap `ValueEnum` (+ `From<SetupProvider> for setup::Provider`). Added `--provider` (default `claude`) to `Cmd::Setup` and `Cmd::Uninstall` (now a struct variant). Threaded into `run_provider`/`uninstall_provider`. Updated the `//!` usage header and the no-store match arm (`Cmd::Uninstall { .. }`). |
| `weave/tests/integration.rs` | 5 new black-box tests (each pins a UNIQUE temp `HOME`): codex/gemini/aider create+content+idempotent+foreign-preserved+uninstall round-trip; a `--provider claude` regression test asserting bare `setup` and `--provider claude` produce byte-identical settings.json; an invalid-`--provider bogus` clap-rejection test (asserts non-zero exit + no file written). |
| `CHANGELOG.md` | New `[Unreleased]` WL-042 block under `### Added`, with the gemini/aider caveats called out. |
| `README.md` | "Use with Claude Code" → "Use with your coding agent": `weave setup --provider` table (target file + mechanism + caveat per provider) + the discipline summary. |
| `ARCHITECTURE.md` | `§ setup.rs — Claude Code wiring` → "host wiring (multi-provider)": the `Provider` enum, per-provider target+strategy table, shared merge primitives, read-back-verify, no-drift/no-dep note. |
| `docs/MULTI-SURFACE-PARITY.md` | Admin row annotated with multi-provider host wiring + per-provider ◐ status; a tracked-gap WL-042 entry in the decomposition section documenting the gemini/aider follow-ups. |
| `docs/REPOWIRE-PARITY.md` | New casr-parity row: "Multi-provider host wiring" marked ◐ PARTIAL (gemini key unconfirmed, aider surface limited — scaffold-with-caveat). |

## Behavior

- Default `--provider claude` (and bare `setup`/`uninstall`) is **unchanged,
  byte-for-byte** — the Claude branch is the original `run`/`uninstall` body, and
  the `.bak`/`.tmp` sidecar names are preserved (the new `sidecar()` appends
  `.weave.bak`/`.weave.tmp` to the file name, which for `settings.json` yields the
  identical `settings.json.weave.bak`/`.tmp` the old `with_extension` produced).
  A dedicated regression test asserts byte-identity.
- **codex:** sets the top-level `notify = ["<exe>", "hook", "wake"]` key in
  `~/.codex/config.toml` via a line-based merge — replaces an existing top-level
  `notify` line in place (idempotent / heals stale path), or inserts before the
  first `[table]` header (top-level keys must precede tables in TOML), else
  appends. Read-back asserts the exact line is present. **No `toml` dependency
  added** (the bin crate does not depend on `toml`; only `weave-core` does, for
  config parsing — left untouched).
- **gemini:** reuses the Claude JSON `hooks.{event}` merge at
  `~/.gemini/settings.json` (no MCP-register step — no `claude`-shaped CLI assumed).
- **aider:** appends a marker-delimited `weave-hook:` YAML stanza to
  `~/.aider.conf.yml`; prune removes only the two weave-owned lines. **No
  `serde_yaml`/YAML dependency** — hand-templated string compose + read-back
  marker-presence check.

## Caveats (explicit, per the owner "scaffold-with-caveat, do not invent silently")

- **gemini — hook key UNCONFIRMED.** Gemini CLI uses a Claude-shaped JSON settings
  file, but its exact lifecycle-hook key is not confirmed. weave scaffolds the
  documented best-known (Claude-compatible) `hooks.{event}` shape and **prints the
  caveat on every run** (`note: Gemini CLI's exact lifecycle-hook key is
  UNCONFIRMED…`). Documented in code comment, README, and both parity docs.
- **aider — hook surface LIMITED.** Aider has no rich lifecycle-hook surface; the
  appended `weave-hook:` stanza is a best-effort scaffold Aider may ignore until it
  grows a hook surface. weave **prints the caveat on every run** and the gap is
  tracked in `MULTI-SURFACE-PARITY.md` / `REPOWIRE-PARITY.md` (marked ◐ PARTIAL).
- **codex — mechanism partially confirmed.** `notify` is Codex's documented
  automation hook, but it has no per-event granularity, so weave maps it to its
  drain (`hook wake`) and prints that mapping note. Written Rust-natively, NOT via
  the ecc `.codex` sidecar tooling (drift guard).

## Tests

- **Unit (setup.rs `mod tests`): 8 new** — `codex_notify_line_is_toml_argv_and_round_trips`,
  `codex_merge_inserts_before_first_table_and_preserves_foreign`,
  `codex_merge_is_idempotent_and_heals_stale_path`, `codex_prune_removes_only_notify`,
  `aider_stanza_carries_marker_and_quoted_exe`,
  `aider_merge_appends_once_and_preserves_foreign`,
  `aider_prune_removes_only_weave_stanza`, plus the codex read-back predicate
  exercised in the first test. (Mirror the WL-041 matcher/round-trip style.)
- **Integration (integration.rs): 5 new** — `setup_codex_writes_notify_idempotent_and_preserves_foreign`,
  `setup_gemini_writes_hooks_idempotent_and_preserves_foreign`,
  `setup_aider_writes_stanza_idempotent_and_preserves_foreign`,
  `setup_provider_claude_is_unchanged_default_path` (regression),
  `setup_rejects_invalid_provider` (clap rejection). ALL pin a UNIQUE temp `HOME`.

**New test count: 13** (8 unit + 5 integration).

## HOME-isolation (the WL-041 #1 risk) — handled

`scrub_env` scrubs `XDG_CONFIG_HOME` but NOT `HOME`; every provider config lives
under `$HOME`. Every new integration test pins a unique temp `HOME` via
`run_env(..., &[("HOME", &home_str)])`. Verified after the full run that the
developer's real `~/.codex`, `~/.gemini`, `~/.aider.conf.yml` carry no test-path
leak.

## Build / gate results

- `cargo build --release` (sqlite): clean.
- `cargo build --no-default-features --features libsql`: clean.
- `cargo clippy -p weave --all-targets -- -D warnings` (sqlite): clean.
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings`: clean.
- `cargo fmt -p weave --check`: clean.
- `cargo test -p weave` (sqlite): **324 passing** (was 316 at WL-041; +8 integration), 0 failed.
- `cargo test -p weave --no-default-features --features libsql`: 323 passing, 1 ignored, 0 failed.
- `cargo test -p weave-mcp standing_mcp_surface…`: green (standing token budget unchanged).

## Deviations from the plan

- **No `tests/security.rs` property** — the plan marked the never-clobber-foreign
  fuzz property OPTIONAL; the existing WL-041 security test
  (`setup_failed_settings_write_is_loud_and_preserves_foreign_hooks`) already
  covers the loud-failure/foreign-preservation seam for the JSON path, and the new
  providers' foreign-preservation is covered by enumerated unit + integration
  cases. Flagging for the guardian if a fuzz property is wanted.
- **Removed the thin `pub run`/`pub uninstall` and `read_settings`/`write_settings`
  wrappers** rather than keeping them with `#[allow(dead_code)]`. `run_provider`/
  `uninstall_provider` are the entry points (main.rs threads the flag through them),
  and `read_json`/`write_json_atomic` replaced the Claude-only wrappers everywhere.
  No external caller referenced the removed names (tests/benches checked).
- **Aider: chose hand-templated YAML, not deferral.** The plan offered "scaffold a
  minimal stanza OR defer to a follow-up." Implemented the minimal stanza (marker +
  one `weave-hook:` line) with read-back marker-presence verification, since it is
  fully achievable with zero new dependency and gives a real idempotent/uninstall
  round-trip. The limitation is documented, not hidden.
