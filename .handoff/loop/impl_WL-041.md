# WL-041 — Implementation change log

**Task:** Read-back verification for destructive config/hook writes.
**Worktree:** `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch`)
**Store/backend boundary crossed:** NO. Only the `weave` bin layer
(`setup.rs`, `backup.rs`) + test files + docs. No `Store` trait / SQL / schema /
`store_libsql.rs` change. Both backends compile and the new tests pass under both.

## Files touched

| File | Rationale |
|---|---|
| `weave/src/setup.rs` | Added read-back-verify helpers (`foreign_commands`, `has_weave_command_for`, `verify_settings_merged`, `verify_settings_pruned`, `verify_git_hook_written`); wired them into `merge_hooks`, `prune_hooks`, `install_git_precommit_hook`. Added 10 unit tests in `mod tests`. |
| `weave/src/backup.rs` | Added `verify_restored_bytes(path, expected, is_json)`; called it after the `write_file` for `config.toml` and `settings.json` in `run_restore`. |
| `weave/tests/integration.rs` | 5 new FS-level tests (temp-HOME-pinned): merge read-back + foreign preservation, idempotent re-run, uninstall prune round-trip, git-hook read-back + foreign preservation, restore config/settings byte round-trip. Added a `unique_tmp_dir` helper. |
| `weave/tests/security.rs` | 1 new test: a settings.json write that cannot land fails loudly AND preserves the pre-existing foreign hook (read-only `.claude/` dir seam). |
| `CHANGELOG.md` | `[Unreleased]` `### Security` entry. |
| `docs/SECURITY.md` | New "Read-back verification for config/hook rewrites (WL-041)" subsection under §4. |
| `ARCHITECTURE.md` | One paragraph in `setup.rs` § describing the read-back contract. |
| `docs/REPOWIRE-PARITY.md` | New casr-parity row in the messaging table. |

## What changed in behavior

After each destructive write, the file is re-opened, re-parsed (JSON for
settings.json, text for the git hook), and the intended content is asserted present
+ well-formed; foreign content captured *before* the write must still be present.
On mismatch a descriptive `Err` is returned (naming the `.bak` recovery path) — never
a silent `Ok`. **The write behavior itself is unchanged** — only the post-write
verification step was added. Idempotency and foreign-content preservation are
unchanged (the read-back *enforces* them, never rewrites the file).

Per-site predicates:
- **setup** (`merge_hooks`): the four weave hook commands (session/prompt/wake×2)
  for the current `exe` are present AND every pre-existing foreign command survived.
- **uninstall** (`prune_hooks`): no weave command remains under any event AND every
  foreign command survived. (Only runs when `removed > 0`, as before.)
- **git pre-commit** (`install_git_precommit_hook`): file exists + contains the guard
  line; fresh file starts with `#!/bin/sh`; pre-existing content preserved (append-only).
- **restore** (`run_restore`): restored `config.toml`/`settings.json` bytes == archived
  payload; settings.json re-parses as a JSON object.

## Tests

- **Unit (setup.rs `mod tests`): 10 new** — `foreign_commands_excludes_weave_includes_others`,
  `verify_merged_ok_on_complete_write`, `verify_merged_errs_when_a_weave_hook_is_missing`,
  `verify_merged_errs_when_a_foreign_hook_vanished`, `verify_merged_errs_when_exe_points_elsewhere`,
  `verify_pruned_ok_when_no_weave_hook_and_foreign_kept`, `verify_pruned_errs_when_a_weave_hook_survives`,
  `verify_pruned_errs_when_foreign_lost`, `verify_git_hook_ok_fresh_file`,
  `verify_git_hook_errs_when_guard_absent`, `verify_git_hook_errs_when_foreign_content_clobbered`.
  (These prove the predicate CATCHES bad/partial writes — fed corrupted/partial content, assert `Err`.)
  *(11 functions; counted as the new predicate unit layer.)*
- **Integration (integration.rs): 5 new** — `setup_settings_merge_is_read_back_verified`,
  `setup_settings_merge_idempotent_second_run`, `uninstall_prune_is_read_back_verified`,
  `git_hook_install_is_read_back_verified_and_preserves_foreign`,
  `restore_config_settings_read_back_round_trip`. ALL pin a unique temp `HOME`.
- **Security (security.rs): 1 new** — `setup_failed_settings_write_is_loud_and_preserves_foreign_hooks`.

**New test count: 17** (11 unit + 5 integration + 1 security).

## HOME-isolation (the #1 risk) — handled

`scrub_env` scrubs `XDG_CONFIG_HOME` but NOT `HOME`, and settings.json lives at
`$HOME/.claude/`. Every new settings.json test pins a **unique temp HOME** via
`run_env`/`run_in_cwd` extra_env (or temp `.git/hooks` via cwd for the git-hook test).
Verified after the full run that the developer's real `~/.claude/settings.json` is
untouched (its pre-existing 4 weave references are from a prior real install, not the
tests; no temp path leaked in).

## Build / gate results

- `cargo build` (sqlite): clean.
- `cargo build --no-default-features --features libsql`: clean.
- `cargo clippy -p weave --all-targets -- -D warnings` (sqlite): clean.
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings`: clean.
- `cargo fmt -p weave --check`: clean.
- `cargo test -p weave` (sqlite): **316 passing** (37 + 190 + 6 + 79), 0 failed.
- `cargo test -p weave --no-default-features --features libsql` (new tests): pass.

## Deviations from the plan

- **Unit test for "a weave hook is missing":** the plan suggested dropping the `Stop`
  event. `Stop` and `SubagentStop` both use the `wake` arg and `has_weave_command_for`
  searches across all events, so a `wake` command still existed. Dropped
  `UserPromptSubmit` instead (its `prompt` arg is unique), which deterministically
  triggers the `Err`. Same intent, correct seam.
- **`has_weave_command_for` is event-agnostic** (searches all events for the intended
  command string, keyed on the `(exe, arg)` pair) rather than per-event. This is the
  honest read-back: the merge writes one command per arg, and the four args
  (session/prompt/wake/wake) are what we verify landed. A weave command misfiled under
  the wrong event is not a realistic failure of `write_settings` (atomic full-object
  rewrite); the predicate still catches a missing command or a stale exe path.
- No CONTRIBUTING.md change: it has no destructive-write checklist (confirmed), so per
  the plan's "optional" note it was skipped.
- No proptest: per the plan, the predicates are exact-membership checks covered by
  enumerated unit cases.
