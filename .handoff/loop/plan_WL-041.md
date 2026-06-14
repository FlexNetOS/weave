# WL-041 — Read-back verification for destructive config/hook writes

**Status:** plan (not implemented)
**Worktree:** `/home/drdave/Desktop/meta/weave-wl038-042` (branch `wl-038-042-batch`)
**Parity:** cross_agent_session_resumer (casr) "verify before declaring success"
**Reuses:** WL-035 backup read-back-verify pattern; overlaps WL-040 export read-back.

## Goal

Any operation that REWRITES a config or hook file must READ BACK what it wrote and
confirm the intended content is actually present and well-formed BEFORE reporting
success — never trust the write blindly. weave already does this for the backup
archive (WL-035: re-opens the written archive, re-counts the snapshot inside it).
WL-041 generalizes that discipline to the **setup/hook write paths**: the
`settings.json` hook merge (`weave setup`), the hook prune (`weave uninstall`), and
the git pre-commit hook install (`weave setup --git-hooks`, WL-030). After each
write, re-open the file, parse it (JSON for settings, text-line for the git hook),
assert weave's intended entries are present (merge) or absent (prune) AND that every
pre-existing foreign hook (rtk, repowire, …) survived, and on mismatch return a
descriptive `Err` instead of a silent `Ok`.

## Grounding findings (read before implementing)

- **`config.toml` is load-only.** `weave-core/src/config.rs` only *reads* config
  (`Config::load` → `read_to_string(config_path())` + env overlay). There is **no
  config.toml write path in the running binary** — the only writer is
  `weave restore` in `weave/src/backup.rs:248` (`write_file(&config_path, &c.data)`),
  which is already inside the WL-035 read-back-verified flow for the DB but does
  **not** read-back-verify the restored `config.toml`/`settings.json` byte payloads.
  **=> `config.rs` needs NO change.** The config-write read-back belongs in
  `backup.rs::run_restore` (see Touched files), not config.rs.
- **The real destructive writers are all in `weave/src/setup.rs`:**
  - `merge_hooks(exe)` → `write_settings(&settings)` (the idempotent settings.json
    hook merge). Returns `Ok(added)` immediately after `write_settings` with **no
    read-back**.
  - `prune_hooks()` → `write_settings(&settings)` (uninstall). Same: `Ok(removed)`
    with no read-back.
  - `install_git_precommit_hook(exe)` → appends to `.git/hooks/pre-commit`. Returns
    `Ok(())` after `writeln!` with **no read-back**.
- `write_settings` is already atomic (tmp + rename) and already drops a one-time
  `.bak`; the gap is purely the **post-write read-back**, exactly as WL-035 added it
  to the archive write.
- **Test-harness gap (critical):** `weave/tests/common/mod.rs::scrub_env` scrubs
  `XDG_CONFIG_HOME` but does **NOT** set or scrub `HOME`. `settings_path()` =
  `$HOME/.claude/settings.json`. The existing `cli_setup_git_hooks_installs_pre_commit`
  test sidesteps HOME by writing into a temp git repo's `.git/hooks/` via `cwd`. Any
  new settings.json test MUST pin `HOME` to a temp dir via `run_in_cwd_env` /
  `run_env` `extra_env` (`[("HOME", temp)]`) so it never touches the developer's real
  `~/.claude/settings.json`. (Use a unique temp HOME per test, like the existing
  `XDG_CONFIG_HOME`-isolation signing tests at integration.rs:3469.)

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave/src/setup.rs` | `weave` (bin) | Add a private `verify_settings_written(expected_events, exe)` read-back helper; call it at the end of `merge_hooks` (assert weave hooks present + foreign hooks preserved) and `prune_hooks` (assert weave hooks absent + foreign preserved). Add `verify_git_hook_written(hook_path, guard_line)` read-back; call it at end of `install_git_precommit_hook`. | The three destructive writers currently `Ok(..)` without re-reading. |
| `weave/src/backup.rs` | `weave` (bin) | In `run_restore`, after `write_file(&config_path, …)` / `write_file(&settings_path, …)`, read the files back and assert their bytes equal the archived payload (and settings.json re-parses as a JSON object). | restore writes config/settings blindly; bring it under the same read-back contract. |
| `weave/tests/integration.rs` | test | New tests: settings.json merge read-back, foreign-hook preservation, prune read-back, git-hook read-back. | CLI-flag / hook behavior ⇒ integration layer (docs/TESTING §8). |
| `weave/tests/security.rs` | test | New test: corrupt/partial-write simulation proves read-back catches a bad write (returns Err, foreign hooks not destroyed). | security/resource property ⇒ security.rs. |
| `weave-core/src/setup.rs`? | — | **N/A** — setup lives in the `weave` bin crate, not weave-core. |

## Dual-backend?

**NO.** This change touches only `weave/src/setup.rs` and `weave/src/backup.rs` (the
`weave` bin layer) and JSON/text file I/O. It does **not** touch the `Store` trait,
SQL, schema, or either store backend, so `store.rs` / `store_libsql.rs` need no
mirrored edit. (The restore read-back asserts file *bytes* and JSON shape, not store
contents — the DB snapshot is already verified by WL-035's `verify_db_at`, which IS
backend-aware but is unchanged here.) The new tests should still pass under
`--no-default-features --features libsql` since they drive the binary black-box; CI's
libsql job will exercise them unchanged.

## Read-back contract (the verification predicate per site)

### `merge_hooks` (settings.json merge — weave setup)
After `write_settings(&settings)?`, re-open `settings_path()` and:
1. It exists and re-parses as a JSON **object** (reuse `read_settings()` — it already
   bails on a non-object / parse error).
2. For **each** of weave's four `HOOKS` events, `find_weave_command(...)` finds a
   command equal to the `hook_command(exe, arg)` we intended (a weave entry is
   present and points at the current exe). Factor a non-mut `find_weave_command`
   (or reuse `is_weave_command` over the event's entries) so the read-back doesn't
   need `_mut`.
3. **Foreign-hook preservation:** capture the set of foreign (non-weave) inner
   `command` strings BEFORE the merge; after read-back, assert that set is a subset
   of the foreign commands still present. (i.e. the merge added/healed only weave
   entries and clobbered nothing.)
   - On any failure: `bail!("settings.json read-back verification failed: <what was
     missing/lost>")` — the `.bak` snapshot from `write_settings` is the recovery
     path; name it in the error.

### `prune_hooks` (settings.json prune — weave uninstall)
After `write_settings(&settings)?` (only runs when `removed > 0`), re-open and:
1. Re-parses as a JSON object.
2. **No** weave command remains under any event (`is_weave_command` matches nothing).
3. Foreign-hook preservation: same captured-foreign-set subset assertion.
   - On failure: descriptive `bail!`.

### `install_git_precommit_hook` (git pre-commit — weave setup --git-hooks)
After the `writeln!`s, drop the file handle, re-read the hook file and:
1. It exists and `contains(&guard_line)` (the exact `'<exe>' lease guard` line we
   wrote) — proves the append landed.
2. If we created it fresh, it starts with the `#!/bin/sh` shebang.
3. **Foreign preservation:** if `existing` was non-empty, assert the re-read content
   still `contains` the pre-existing content prefix (we only appended; the
   install-preflight "never clobber a foreign hook" rule). Capture `existing` before
   the open and assert `reread.starts_with(&existing)` (after the trailing-newline
   normalization the code already does, so compare against `existing` with the same
   newline fixup).
   - On failure: `bail!("pre-commit hook read-back verification failed: …")`.

### `run_restore` (backup.rs — config.toml / settings.json restore)
After each `write_file(&config_path, &c.data)` / `write_file(&settings_path, &s.data)`:
1. Read the file back; assert bytes `==` the archived payload (`c.data` / `s.data`).
2. For settings.json specifically, assert the re-read bytes parse as a JSON object.
   - On mismatch: `bail!("restore read-back failed for <path>")` (the WL-035
     `backup_existing` `.bak` is the recovery path).

## Invariants in scope

- **No shell, ever** (`setup.rs`): the read-back is pure file I/O + serde_json /
  string `contains`; no new `Command`. The git-hook guard line is still
  `shell_single_quote`d — unchanged.
- **stdout discipline** (`setup.rs`/`backup.rs` are CLI, not MCP, so `println!` is
  fine; a read-back *failure* must go through `Err`/`anyhow`, not a stray stdout
  line). No JSON-RPC surface touched, so MCP stdout discipline is not at risk.
- **Idempotency + non-destruction of foreign content** (`setup.rs`): the read-back
  *enforces* the existing idempotency/foreign-preservation guarantees rather than
  weakening them — it must never itself rewrite the file (read-only verification).
- **Atomicity preserved** (`setup.rs`): the read-back runs AFTER the existing atomic
  tmp+rename in `write_settings`; do not move the rename or add a second write.
- **Input caps**: unchanged (no new user-text path).
- **Token-light MCP surface**: unchanged — **no new MCP tool, no new standing tool.**
  This is CLI-only behavior; nothing added to `tool_catalog()` or `tools/list`.

## Test layers required

Per `docs/TESTING.md` §8:

1. **Integration (`tests/integration.rs`)** — pin a temp `HOME` (extra_env) for all:
   - `setup_settings_merge_is_read_back_verified`: seed `$HOME/.claude/settings.json`
     with a FOREIGN hook (e.g. an `rtk` command under `SessionStart`), run
     `weave setup` (MCP register will no-op without `claude` on PATH — fine), assert
     exit 0, then read the file and assert (a) weave's four hooks present, (b) the
     foreign rtk hook still present. This proves the read-back passes on a good write
     AND foreign preservation.
   - `setup_settings_merge_idempotent_second_run`: run setup twice; second run still
     verifies (no duplicate weave entry, foreign intact).
   - `uninstall_prune_is_read_back_verified`: after setup, run `weave uninstall`;
     read back and assert no weave hook remains and the foreign rtk hook survived.
   - `git_hook_install_is_read_back_verified`: extend/parallel the existing
     `cli_setup_git_hooks_installs_pre_commit` — seed the pre-commit file with a
     foreign line first, run `setup --git-hooks`, assert read-back confirms BOTH the
     guard line and the pre-existing foreign line (preservation).
   - `restore_config_settings_read_back` (overlaps WL-040): `weave backup` then
     `weave restore` into a temp HOME/XDG; assert restored config.toml + settings.json
     bytes match what was backed up (round-trip read-back).
2. **Security (`tests/security.rs`)**:
   - `setup_read_back_catches_corrupted_settings_write`: simulate a bad write by
     making the post-write file unreadable/corrupt and assert setup returns a
     non-zero exit with a descriptive "read-back verification failed" message AND
     the foreign hook is not destroyed. (Realizable seam: point `HOME` at a temp
     dir, pre-seed a foreign-hook settings.json, then make `~/.claude/settings.json`
     a path the process can write a tmp next to but where re-read yields unexpected
     content — e.g. by racing is hard; simpler: add a `#[cfg(test)]`-free unit-style
     test in `setup.rs` calling the verify helper directly with a hand-built
     mismatched `Value`, asserting it `Err`s. Prefer a **unit test in `setup.rs`'s
     `mod tests`** for the predicate itself, plus the integration happy/foreign-path
     tests above.)
   - **Recommended split:** put the *predicate* correctness (verify helper returns
     Err on a missing weave hook / lost foreign hook, Ok on a good merge) as **unit
     tests in `setup.rs` `mod tests`** (pure `serde_json::Value` inputs, no FS, no
     HOME) — this is the cleanest "corrupt-write" simulation and needs no temp HOME.
     Keep the FS-level happy-path + foreign-preservation in integration.
3. **Unit (`setup.rs` `mod tests`)**: the new `verify_settings_written` /
   `verify_git_hook_written` predicates over crafted `Value`/string inputs:
   asserts Err when a weave hook is missing, Err when a foreign command vanished,
   Ok on a correct merge, Ok-empty on a correct prune.
4. **Proptest:** **not required** — no new pure invariant over arbitrary input
   (the predicates are exact-membership checks, covered by enumerated unit cases).

## Docs to sync

- **`CHANGELOG.md`** `[Unreleased]`: add under a `### Security` / `### Changed`
  entry — "setup/uninstall/git-hook installs and restore now **read-back-verify**
  config/hook writes before reporting success (WL-041, casr parity); a write whose
  re-read does not contain the intended weave entries — or that lost a pre-existing
  foreign hook — now fails loudly instead of silently succeeding."
- **`docs/SECURITY.md`**: add a short note in the destructive-ops / setup section
  that every config/hook **rewrite** is read-back-verified (the file is re-opened,
  re-parsed, and weave's intended entries + foreign-hook preservation are confirmed
  before success), mirroring the WL-035 backup read-back guarantee.
- **`ARCHITECTURE.md`**: one line in the setup/§ "destructive writes" description
  noting the read-back-verify step (re-open + assert merged entries present + foreign
  preserved), referencing WL-035 as the established pattern.
- **`docs/REPOWIRE-PARITY.md`**: add/extend the read-back row — currently only the
  backup row (line ~147) mentions read-back-verify; add a casr-parity row:
  "Verify-before-success on destructive config/hook rewrites | setup/uninstall/git-hook
  + restore read-back-verify | ✅ HAVE | WL-041".
- **`CONTRIBUTING.md`**: only if it documents a "destructive write" checklist; if so,
  add "read-back-verify the write" to it. (Optional — confirm during implementation.)

## Edit order

1. `weave/src/setup.rs`: add `verify_settings_written` + `verify_git_hook_written`
   helpers and a non-mut foreign-command capture util; wire into `merge_hooks`,
   `prune_hooks`, `install_git_precommit_hook`. Add unit tests in `mod tests`.
2. `weave/src/backup.rs`: add the config/settings read-back to `run_restore`.
3. `weave/tests/integration.rs`: add the happy-path + foreign-preservation +
   idempotency + restore round-trip tests (temp HOME pinned).
4. `weave/tests/security.rs`: add the corrupt-write-caught test (or confirm the unit
   predicate tests in step 1 cover it and add a thin FS-level security assertion).
5. Docs: CHANGELOG, SECURITY, ARCHITECTURE, REPOWIRE-PARITY (and CONTRIBUTING if a
   checklist exists).
6. Gate: `cargo build --release`, `cargo test --all-targets`,
   `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`, plus the
   libsql trio (build/clippy/test `--no-default-features --features libsql`) since
   the new black-box tests run there too.

## Risks / open questions

1. **HOME isolation in tests (must-fix):** the harness scrubs `XDG_CONFIG_HOME` but
   NOT `HOME`. Every settings.json test MUST pass a unique temp `HOME` via
   `extra_env`, or it will read/write the developer's real `~/.claude/settings.json`.
   Implementer: use `run_in_cwd_env`/`run_env` with `[("HOME", &temp)]`; do NOT add
   HOME to `scrub_env` globally (other tests may rely on the real HOME being absent
   ≠ present). Confirm `setup::run` reads `HOME` fresh per process (it does —
   `home()` calls `std::env::var("HOME")`).
2. **MCP register side effect in `setup::run`:** `run()` calls `register_mcp` which
   shells out to `claude`. In CI/tests `claude` is absent → it prints a note and
   continues (already best-effort). Read-back tests rely on this graceful skip; no
   change needed, but the implementer should assert on the **hooks** outcome, not the
   MCP line.
3. **Corrupt-write simulation realizability:** a true "the OS wrote garbage" race is
   hard to force deterministically at the integration layer. Recommended resolution
   (in the plan): make the *predicate* the unit-test target (hand-built mismatched
   `Value`) — that is the honest "read-back catches a bad write" proof — and keep the
   FS tests for the happy/foreign paths. Implementer to confirm this satisfies the
   guardian's "prove read-back catches a bad write" requirement; if a stronger
   end-to-end corrupt-write test is wanted, inject the failure behind a
   `#[cfg(test)]`-gated hook (avoid shipping test-only branches in `setup.rs` if
   possible — prefer the unit predicate).
4. **Restore read-back scope (WL-040 overlap):** WL-040 covers export read-back; this
   plan adds restore's config/settings read-back. Confirm with the leader these don't
   collide — if WL-040 already added a `restore` read-back, fold rather than
   duplicate. Backup's DB read-back is already done (WL-035) — do NOT re-add it.
5. **`find_weave_command` refactor:** `find_weave_command_mut` is `&mut`; the read-back
   needs a read-only view. Add a small non-mut `find_weave_command(&[Value]) -> bool`
   (or iterate with `is_weave_command`) rather than calling the `_mut` variant on a
   freshly-parsed `Value`. Low blast radius (new private fn).
