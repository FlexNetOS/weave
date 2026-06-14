# WL-035 — Mailbox backup / restore (SQLite snapshot + config + hooks)

Plan file: `/tmp/wl035_plan.md`
Base: read-only against `/home/drdave/Desktop/meta/weave` (develop @ 306dcd3). No code edited, no git.

## Goal

Add two CLI subcommands. `weave backup --out <path>` produces a single portable,
**dependency-free** archive snapshotting: (1) a *consistent* copy of the weave
SQLite store (via `VACUUM INTO`, never a raw file copy), (2) the `config.toml`,
and (3) the installed Claude wiring (`~/.claude/settings.json`, the file weave's
`setup.rs` merges its hooks into). `weave restore --in <path>` extracts the
archive back, restoring the DB / config / settings with a hardened, traversal-safe
extractor and **read-back verification** at both ends (atm-core parity; WL-041
verify-the-write spirit). No new heavyweight dependency, no shell, no new standing
MCP tool.

## Format decision (HARD CONSTRAINT 1) — hand-rolled no-dep uncompressed `tar` (option **a**)

**Choice: a minimal, uncompressed USTAR (POSIX tar) writer + reader implemented in
pure Rust in a new `weave-core/src/archive.rs` module. ZERO new dependencies.**

- `cargo tree` impact on the **default build: ZERO.** No `tar`, no `zip`, no
  `flate2`/`miniz`. Tar is a trivial 512-byte-header block format (name, mode,
  size octal, checksum, typeflag, then file data zero-padded to 512), which is why
  it is the right no-dep choice over `zip` (zip mandates a compression/CRC tree).
- Uncompressed is acceptable: a weave store is small (messages capped at 64 KiB,
  GC'd at 30 days) and the archive is a portability/transport container, not a
  space optimization. Compression, **if ever** wanted, belongs behind a NON-default
  feature flag (option b) exactly like `libsql`/`sign` — explicitly out of scope
  here to keep the default build dep-free.
- Rejected (c) ad-hoc concatenation: a real (if minimal) tar format gives us a
  self-describing, `tar tf`-inspectable artifact for free and a well-understood
  traversal-guard surface, with no more code than a bespoke format.

USTAR subset implemented (enough, nothing more):
- Writer: `typeflag '0'` (regular file) entries only; fields name (≤100 bytes —
  our entry names are short fixed strings, see manifest), `mode`, `size` (octal),
  `mtime` (octal, may be 0), checksum, `magic = "ustar\0"`, `version = "00"`; two
  512-byte zero blocks as the end-of-archive marker.
- Reader: parse header blocks, validate the checksum, read `size` bytes of body,
  skip padding; stop at the zero-block terminator.

## Touched files

| File | Layer | What changes | Why |
|---|---|---|---|
| `weave-core/src/archive.rs` **(new)** | `model`/pure (no I/O beyond byte buffers) | Pure USTAR writer/reader: `ArchiveEntry`, `write_archive(entries) -> Vec<u8>`, `read_archive(&[u8]) -> Result<Vec<ArchiveEntry>>`, plus the **traversal guard** `safe_entry_name(name) -> Result<&str>` (rejects absolute, `..`, NUL, embedded `/` beyond the fixed manifest names). | No-dep portable container; pure so it unit-tests with no filesystem. |
| `weave-core/src/lib.rs` | wiring | `pub mod archive;` | Expose the module. |
| `weave-core/src/store.rs` | `store` (sqlite) | Add `fn snapshot_to(&self, dest: &Path) -> Result<()>` to the `Store` trait; impl on `SqliteStore` issues `VACUUM INTO ?1` (bound param) then a **read-back open + count** sanity check. | Consistent snapshot; trait method so both backends mirror it. |
| `weave-core/src/store_libsql.rs` | `store` (libsql) | Mirror `snapshot_to` on `LibsqlStore` (local-file path only; see Dual-backend). | Both builds must compile + behave. |
| `weave/src/main.rs` | `main` | Add `Cmd::Backup { out, force }` and `Cmd::Restore { in_path, force }`; new `backup.rs` glue module orchestrating snapshot→assemble→verify and verify→extract→restore. | CLI surface + orchestration. |
| `weave/src/backup.rs` **(new)** | `main`/`setup` layer | `run_backup(cfg, out, force)` and `run_restore(cfg, in_path, force)`: enumerate sources, call `Store::snapshot_to` into a temp file, read config + settings paths, build the archive via `archive::write_archive`, write to `--out`, **read it back** (`read_archive`) to confirm it parses + DB entry opens. Restore: read+parse archive, validate every entry name through `safe_entry_name`, write DB to a temp path, **open it read-back and assert row counts sane**, then atomically move into place (refuse to overwrite an existing DB/config/settings unless `--force`). | One orchestration seam; keeps `main.rs` thin. |
| `weave/src/main.rs` (dispatch) | `main` | Wire the two `Cmd` arms to `backup::run_backup` / `run_restore`. | Dispatch. |

Layer DAG respected: `archive.rs` is pure (sits with `model`); `snapshot_to` is in
`store`; orchestration is in the `main` crate (`weave/src/backup.rs`), which may
read `config::config_path()` / `setup`'s settings path. No upward dep.

## What goes in the archive (fixed manifest — entry names are constant strings)

| Entry name | Source | Notes |
|---|---|---|
| `messages.db` | `Store::snapshot_to(tmp)` output (`VACUUM INTO`) | The consistent DB snapshot. Always present. |
| `config.toml` | `weave_core::config::config_path()` | Omitted if the file does not exist (record absence in manifest). |
| `settings.json` | `~/.claude/settings.json` (`setup::settings_path()` — currently private; expose a `pub fn settings_path()` or a small `pub fn installed_hook_files() -> Vec<PathBuf>`). | This **is** weave's "installed hooks": `setup.rs` does NOT drop standalone hook scripts — it MERGES hook entries (`SessionStart→session`, `UserPromptSubmit→prompt`, `Stop/SubagentStop→wake`) into `settings.json`. Backing up `settings.json` captures the installed hooks. Omitted if absent. |
| `MANIFEST` (text) | generated | Lists which optional members are present + weave version + backend, so restore knows what to expect and can warn on a partial archive. |

**Note for implementer:** the MCP registration (`claude mcp add`, lives in
`~/.claude.json`) is OUT of scope — it is re-creatable via `weave setup` and is not
a weave-owned file we should rewrite on restore. Document that restore does not touch
MCP registration; the user re-runs `weave setup` if needed. (Open question Q3.)

## Snapshot mechanism (HARD CONSTRAINT 2) + dual-backend plan

`Store::snapshot_to(&self, dest: &Path) -> Result<()>`:
- **sqlite (`store.rs`):** `self.conn.execute("VACUUM INTO ?1", params![dest_str])?`
  — parameterized (no SQL literal of a user path), atomic + consistent (writes a
  fully-checkpointed copy; no WAL/torn-write hazard, unlike `fs::copy` of a live
  WAL DB). Then **read-back verify**: `SqliteStore::open_readonly(dest)` and
  `total_messages()` succeeds (snapshot opens + is a valid weave store) before
  returning Ok. `dest` must be a fresh path (VACUUM INTO refuses an existing file)
  — the caller hands it a unique temp path.
- **libsql (`store_libsql.rs`):** mirror via `conn.execute("VACUUM INTO ?1", params)`
  on the **local-file** path. libSQL's bundled SQLite supports `VACUUM INTO`.
  **Fallback / guard:** for a **remote** libsql backend (`cfg.libsql_url.is_some()`)
  there is no local file to vacuum-into a server-side path; `snapshot_to` must
  `bail!` with a clear "backup is not supported for the remote libsql backend
  (snapshot the Turso DB server-side)" rather than silently producing nothing.
  Confirm the local VACUUM INTO path in the libsql build's test; if the bundled
  engine rejected it (it should not), the fallback is the online backup API — note
  this as a contingency, not the primary path.

This keeps the **dual-backend mirror invariant**: a new `Store` trait method is
implemented in BOTH backend files; both `cargo build`/`test` (default sqlite) and
`--no-default-features --features libsql` must stay green.

## Backup + restore flow with read-back verification points (HARD CONSTRAINT 3)

**Backup (`run_backup`):**
1. Resolve `--out`; refuse to overwrite an existing file unless `--force` (path
   validation: reject empty, reject a path whose parent does not exist after a
   non-traversal check).
2. `Store::snapshot_to(tmp_db)` → **verify #1** (snapshot opens read-only + counts).
3. Read `config.toml` + `settings.json` if present.
4. `archive::write_archive(entries)` → write bytes to a temp file next to `--out`,
   `fsync`, atomic rename to `--out`.
5. **Verify #2 (read-back):** re-open `--out`, `read_archive` it, assert the
   `messages.db` entry is present and its bytes open as a valid SQLite store with a
   sane count. Only then print success. Never declare success on an unverified write.

**Restore (`run_restore`):**
1. Read `--in`; `read_archive` (verify it parses; bad checksum / truncated ⇒ error).
2. For EVERY entry, run `safe_entry_name` — **reject `..`, absolute paths, NUL,
   any name not in the known manifest set** (traversal guard, HARD CONSTRAINT 4).
3. Write `messages.db` bytes to a temp path; **read-back verify** (open read-only,
   `total_messages()` succeeds, schema present) BEFORE touching the live store.
4. Refuse to overwrite an existing live DB / config / settings unless `--force`
   (default = safe; explicit intent to clobber). With `--force`, snapshot the
   existing DB to a `.bak` first (mirrors `setup.rs`' `.bak` discipline).
5. Atomically move each verified member into place (`config_path()`,
   `settings_path()`, `db_path()`). Print exactly what was restored.

## Invariants in scope

- **No shell, argv-only** — entire feature is in-process Rust + SQLite C calls; no
  `Command` at all. (`backup.rs`, `archive.rs`, `store*.rs`)
- **Parameterize all SQL** — `VACUUM INTO ?1` binds the path; never inline it.
  (`store.rs`, `store_libsql.rs`)
- **Input is capped / path validation** — `--out`/`--in` validated; archive entry
  names hard-validated against traversal on extract. (`backup.rs`, `archive.rs`)
- **Consistent snapshot** — `VACUUM INTO`, never `fs::copy` of a live DB. (`store*.rs`)
- **Verify-the-write (WL-041)** — read-back at both ends before success. (`backup.rs`, `store*.rs`)
- **stdout discipline** — CLI prints to stdout, errors to stderr; backup/restore are
  NOT on the MCP stdout path. (`main.rs`)
- **Token-light MCP (no new standing tool)** — CLI-only. Optionally surface a
  `backup`/`restore` *catalog* op via the meta-tool `tool_catalog()` later
  (zero standing tokens); NOT a new standing `tools/list` entry. (out of scope here.)
- **Dual-backend mirror** — `snapshot_to` in both store files.

## Test layers required (docs/TESTING.md §8 checklist)

- **Unit (`weave-core/src/archive.rs` `#[cfg(test)]`):**
  - tar round-trip: `write_archive` → `read_archive` returns identical entries
    (names, sizes, bodies byte-identical), including empty body + a member absent.
  - checksum/truncation: a corrupted/truncated buffer is rejected (not a panic).
  - **traversal guard rejection:** `safe_entry_name` rejects `../etc/passwd`,
    `/etc/passwd`, `a/../../b`, an embedded NUL, and any non-manifest name; accepts
    the fixed manifest names.
- **Unit (`weave-core/src/store.rs`):** `snapshot_to` produces a file that opens
  read-only and has the same `total_messages()` as the source; round-trips through
  `open_readonly`. Empty-DB snapshot works (0 messages). (Mirror an equivalent
  libsql test under `cfg(feature="libsql")` in `store_libsql.rs` for the local path,
  plus a remote-backend `bail!` assertion.)
- **Integration (`weave/tests/integration.rs`):** drive the compiled binary —
  `weave send` a couple messages → `weave backup --out X` → fresh `WEAVE_DB` →
  `weave restore --in X` → `weave inbox`/`history` shows the messages survived.
  Assert `--out` refuses to overwrite without `--force`; assert restore refuses to
  clobber an existing DB without `--force`.
- **Security (`weave/tests/security.rs`):** craft (via the pure `archive` API, or a
  hand-built fixture) an archive containing a `../escape` / absolute-path entry and
  assert `weave restore` REFUSES it and writes nothing outside the target dir
  (extraction-traversal guard). Also: a backup `--out` pointing at a path it must not
  overwrite is rejected without `--force`.
- **Prop (`weave/tests/prop.rs`), optional:** property — for arbitrary small sets of
  (name,bytes) entries, `read_archive(write_archive(x)) == x` (round-trip identity).

## Docs to sync

- **README.md** — document `weave backup` / `weave restore` under the CLI section;
  state the no-dep tar format, what the archive contains, and that remote libsql is
  unsupported for snapshot.
- **ARCHITECTURE.md** — add the archive format + snapshot mechanism (VACUUM INTO,
  dual-backend mirror, traversal guard, verify-the-write) to the design notes;
  cross-reference §7 no-shell/parameterized-SQL invariants.
- **CHANGELOG.md** — `[Unreleased]`: "feat(cli): `weave backup`/`restore` — portable
  no-dep snapshot of DB + config + Claude settings (WL-035, atm-core parity)."
- **CONTRIBUTING.md** — only if the new trait method changes the "touch every backend"
  guidance enumeration; otherwise no change (it already states that rule).
- **docs/TESTING.md** — add the new test cases to the layer map if the §8 inventory
  is enumerated per-feature.

## Edit order (dependency-respecting)

1. `weave-core/src/archive.rs` (new) + `lib.rs` `pub mod archive;` + its unit tests
   (pure, no deps; testable in isolation first).
2. `weave-core/src/store.rs`: add `snapshot_to` to the `Store` trait + sqlite impl
   + unit test.
3. `weave-core/src/store_libsql.rs`: mirror `snapshot_to` (local VACUUM INTO +
   remote `bail!`) so the libsql build compiles; libsql unit test.
4. `weave/src/setup.rs`: expose `pub fn settings_path()` (or `installed_hook_files()`).
5. `weave/src/backup.rs` (new): `run_backup` / `run_restore` orchestration + verify.
6. `weave/src/main.rs`: add `Cmd::Backup` / `Cmd::Restore` + dispatch.
7. `weave/tests/integration.rs` + `weave/tests/security.rs` (+ optional `prop.rs`).
8. Docs: README / ARCHITECTURE / CHANGELOG (+ TESTING).
9. Full gate both backends: default sqlite build/test/clippy/fmt, then
   `--no-default-features --features libsql` build/test/clippy.

## Risks / open questions

- **Q1 (empty DB):** `VACUUM INTO` of a freshly-created empty store must succeed and
  restore to an empty-but-valid store — covered by an explicit empty-DB unit test.
- **Q2 (huge DB):** uncompressed tar means archive ≈ DB size; acceptable given GC +
  64 KiB body cap. If a deployment disables GC and the DB is large, the archive is
  large — documented, not blocked. Compression stays a future NON-default feature.
- **Q3 (MCP registration scope):** restore deliberately does NOT rewrite
  `~/.claude.json` (MCP registration) — recommend `weave setup` after restore.
  Confirm with leader this is the desired boundary (atm-core parity may or may not
  include re-registering the server).
- **Q4 (settings.json clobber):** `settings.json` may contain unrelated hooks (rtk,
  repowire) that changed since the backup. Restoring it wholesale could regress those.
  Safer default: restore `settings.json` to a `.restored` sidecar and PRINT a diff
  hint, OR require `--force` to overwrite live settings. Recommend: restore DB+config
  by default; gate `settings.json` overwrite behind `--force` with a `.bak` of the
  current file (mirrors `setup.rs` discipline). **Decide with leader.**
- **Q5 (remote libsql):** no local-file snapshot path; `snapshot_to` bails clearly.
  Confirm that is acceptable vs. attempting the online-backup API server-side.
- **Q6 (entry name ≤100 bytes):** USTAR `name` field is 100 bytes. Our manifest names
  are short fixed constants, so the long-name (PAX/GNU) extension is unneeded — keep
  the writer simple and assert names fit (the implementer must not let a future
  variable-length entry name silently overflow).
