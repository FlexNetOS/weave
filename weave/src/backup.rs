//! WL-035: mailbox backup / restore orchestration.
//!
//! `weave backup --out X` packages a *consistent* SQLite snapshot (via
//! [`Store::snapshot_to`], `VACUUM INTO`, never a raw copy of a live WAL DB),
//! `config.toml`, and weave's installed Claude `settings.json` hooks into one
//! portable, dependency-free USTAR archive (see [`weave_core::archive`]). The write
//! is **read-back-verified** at both ends: after writing, the archive is re-opened
//! and the snapshot inside it is opened + counted; on restore, the extracted DB is
//! opened + sanity-checked BEFORE it touches the live store, and every archive entry
//! name is run through the traversal guard [`archive::safe_entry_name`].
//!
//! Lives in the `weave` (bin) layer: it may read `config::config_path()` /
//! `setup::settings_path()` and call `Store::snapshot_to`. No upward dep.

use anyhow::{bail, Context, Result};
use std::path::Path;
use weave_core::archive::{
    self, ENTRY_CONFIG, ENTRY_DB, ENTRY_MANIFEST, ENTRY_SETTINGS, KNOWN_ENTRY_NAMES,
};
use weave_core::config::{self, Config};
use weave_core::store::Store;

use crate::setup;

/// Write `bytes` to `path`, with an error message naming the path (GAP-2: a bare
/// `std::fs::write` surfaces only "No such file or directory" with no path).
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// WL-041: read-back-verify a just-restored config/settings file. Re-opens `path`,
/// asserts its bytes equal the archived `expected` payload, and (for settings.json,
/// `is_json`) that the re-read content parses as a JSON object — so a restore never
/// reports success on a write that landed truncated or corrupt. The WL-035
/// `backup_existing` `.bak` is the recovery path on failure.
fn verify_restored_bytes(path: &Path, expected: &[u8], is_json: bool) -> Result<()> {
    let got = std::fs::read(path).with_context(|| {
        format!(
            "restore read-back failed for {}: cannot re-read after write",
            path.display()
        )
    })?;
    if got != expected {
        bail!(
            "restore read-back failed for {}: the re-read bytes do not match the archived \
             payload (recover from the .bak)",
            path.display()
        );
    }
    if is_json {
        let v: serde_json::Value = serde_json::from_slice(&got).with_context(|| {
            format!(
                "restore read-back failed for {}: not valid JSON",
                path.display()
            )
        })?;
        if !v.is_object() {
            bail!(
                "restore read-back failed for {}: restored content is not a JSON object",
                path.display()
            );
        }
    }
    Ok(())
}

/// Open the configured backend **read-only** at an arbitrary `path` (a snapshot
/// file), returning a counter we can use to sanity-check it. Backend-aware so the
/// libsql build verifies through libsql.
fn verify_db_at(_cfg: &Config, path: &Path) -> Result<i64> {
    #[cfg(feature = "sqlite")]
    {
        let s = weave_core::store::SqliteStore::open_readonly(path)
            .with_context(|| format!("opening snapshot {} read-only", path.display()))?;
        s.total_messages().context("counting messages in snapshot")
    }
    #[cfg(all(feature = "libsql", not(feature = "sqlite")))]
    {
        let s = weave_core::store_libsql::LibsqlStore::open_readonly(path)
            .with_context(|| format!("opening snapshot {} read-only", path.display()))?;
        s.total_messages().context("counting messages in snapshot")
    }
    #[cfg(not(any(feature = "sqlite", feature = "libsql")))]
    {
        let _ = path;
        bail!("no storage backend compiled in");
    }
}

/// `weave backup --out <out> [--force]`.
pub fn run_backup(cfg: &Config, store: &dyn Store, out: &Path, force: bool) -> Result<()> {
    // --- path validation ---------------------------------------------------
    let out_str = out.as_os_str().to_str();
    if out_str.is_none_or(str::is_empty) {
        bail!("backup --out path is empty or not valid UTF-8");
    }
    if out.exists() && !force {
        bail!(
            "refusing to overwrite existing file {} (pass --force to overwrite)",
            out.display()
        );
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            bail!(
                "backup --out parent directory does not exist: {}",
                parent.display()
            );
        }
    }

    // --- 1) consistent DB snapshot (VACUUM INTO) + verify #1 ---------------
    // A unique temp path next to --out; VACUUM INTO refuses an existing file, so
    // remove any stale temp first.
    let tmp_db = sibling_tmp(out, "weave-snapshot");
    let _ = std::fs::remove_file(&tmp_db);
    store
        .snapshot_to(&tmp_db)
        .context("snapshotting the store (VACUUM INTO + read-back verify)")?;
    let snap_count = verify_db_at(cfg, &tmp_db)?;
    let db_bytes =
        std::fs::read(&tmp_db).with_context(|| format!("reading snapshot {}", tmp_db.display()))?;
    let _ = std::fs::remove_file(&tmp_db);

    // --- 2) optional config.toml + settings.json --------------------------
    let config_path = config::config_path();
    let config_bytes = read_optional(&config_path)?;
    let settings_path = setup::settings_path();
    let settings_bytes = read_optional(&settings_path)?;

    // --- 3) MANIFEST (human/restore-readable membership record) ------------
    let manifest = build_manifest(
        store.backend(),
        snap_count,
        config_bytes.is_some(),
        settings_bytes.is_some(),
    );

    // --- 4) assemble the archive ------------------------------------------
    let mut entries: Vec<(&str, &[u8])> = vec![(ENTRY_DB, &db_bytes)];
    if let Some(c) = &config_bytes {
        entries.push((ENTRY_CONFIG, c));
    }
    if let Some(s) = &settings_bytes {
        entries.push((ENTRY_SETTINGS, s));
    }
    entries.push((ENTRY_MANIFEST, manifest.as_bytes()));
    let bytes = archive::write_archive(&entries).context("building backup archive")?;

    // Write to a temp file next to --out, then atomically rename into place.
    let tmp_out = sibling_tmp(out, "weave-backup");
    write_file(&tmp_out, &bytes)?;
    std::fs::rename(&tmp_out, out)
        .with_context(|| format!("renaming {} -> {}", tmp_out.display(), out.display()))?;

    // --- 5) read-back verify #2: re-open the written archive --------------
    let written = std::fs::read(out)
        .with_context(|| format!("re-reading written archive {}", out.display()))?;
    let parsed =
        archive::read_archive(&written).context("re-parsing the written archive (verification)")?;
    let db_entry = parsed
        .iter()
        .find(|e| e.name == ENTRY_DB)
        .ok_or_else(|| anyhow::anyhow!("written archive is missing {ENTRY_DB}"))?;
    // The snapshot bytes must re-open as a valid store with the expected count.
    let vtmp = sibling_tmp(out, "weave-verify");
    let _ = std::fs::remove_file(&vtmp);
    write_file(&vtmp, &db_entry.data)?;
    let vcount = verify_db_at(cfg, &vtmp);
    let _ = std::fs::remove_file(&vtmp);
    let vcount = vcount.context("verifying the snapshot inside the written archive")?;
    if vcount != snap_count {
        bail!(
            "backup verification failed: snapshot reported {snap_count} messages but the \
             archived copy reports {vcount}"
        );
    }

    println!(
        "backup written: {} ({} message(s){}{})",
        out.display(),
        snap_count,
        if config_bytes.is_some() {
            ", config.toml"
        } else {
            ""
        },
        if settings_bytes.is_some() {
            ", settings.json"
        } else {
            ""
        },
    );
    Ok(())
}

/// `weave restore --in <in_path> [--force]`.
pub fn run_restore(cfg: &Config, in_path: &Path, force: bool) -> Result<()> {
    if in_path.as_os_str().is_empty() {
        bail!("restore --in path is empty");
    }
    let bytes = std::fs::read(in_path)
        .with_context(|| format!("reading backup archive {}", in_path.display()))?;
    let entries = archive::read_archive(&bytes)
        .with_context(|| format!("parsing backup archive {}", in_path.display()))?;

    // --- traversal guard: validate EVERY entry name before using any ------
    for e in &entries {
        archive::safe_entry_name(&e.name).with_context(|| {
            format!(
                "backup archive {} contains an unsafe entry",
                in_path.display()
            )
        })?;
    }

    let db_entry = entries
        .iter()
        .find(|e| e.name == ENTRY_DB)
        .ok_or_else(|| anyhow::anyhow!("archive is missing {ENTRY_DB}; not a weave backup"))?;
    let config_entry = entries.iter().find(|e| e.name == ENTRY_CONFIG);
    let settings_entry = entries.iter().find(|e| e.name == ENTRY_SETTINGS);

    // --- write the DB to a temp path and read-back verify BEFORE touching
    //     the live store -----------------------------------------------------
    let db_path = cfg.db_path();
    let staged_db = sibling_tmp(&db_path, "weave-restore");
    let _ = std::fs::remove_file(&staged_db);
    if let Some(parent) = staged_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_file(&staged_db, &db_entry.data)?;
    let restored_count = verify_db_at(cfg, &staged_db)
        .context("the DB in the archive did not open as a valid weave store");
    if restored_count.is_err() {
        let _ = std::fs::remove_file(&staged_db);
    }
    let restored_count = restored_count?;

    // --- clobber guards (default safe; --force to overwrite) --------------
    if db_path.exists() && !force {
        let _ = std::fs::remove_file(&staged_db);
        bail!(
            "refusing to overwrite existing database {} (pass --force)",
            db_path.display()
        );
    }
    let config_path = config::config_path();
    if config_entry.is_some() && config_path.exists() && !force {
        let _ = std::fs::remove_file(&staged_db);
        bail!(
            "refusing to overwrite existing config {} (pass --force)",
            config_path.display()
        );
    }

    // --- move the verified DB into place ----------------------------------
    if db_path.exists() && force {
        backup_existing(&db_path)?;
    }
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(&staged_db, &db_path)
        .with_context(|| format!("moving restored DB into place at {}", db_path.display()))?;
    let mut restored = vec![format!(
        "{} ({} message(s))",
        db_path.display(),
        restored_count
    )];

    // --- config.toml (default-restored unless it exists w/o --force) ------
    if let Some(c) = config_entry {
        if config_path.exists() && force {
            backup_existing(&config_path)?;
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        write_file(&config_path, &c.data)?;
        verify_restored_bytes(&config_path, &c.data, false)?;
        restored.push(config_path.display().to_string());
    }

    // --- settings.json — ONLY with --force; write a .bak first ------------
    let mut skipped_settings = false;
    if let Some(s) = settings_entry {
        let settings_path = setup::settings_path();
        if force {
            if settings_path.exists() {
                backup_existing(&settings_path)?;
            }
            if let Some(parent) = settings_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_file(&settings_path, &s.data)?;
            verify_restored_bytes(&settings_path, &s.data, true)?;
            restored.push(settings_path.display().to_string());
        } else {
            skipped_settings = true;
        }
    }

    println!("restored:");
    for r in &restored {
        println!("  {r}");
    }
    if skipped_settings {
        println!(
            "  (settings.json present in the archive was NOT restored; pass --force to \
             overwrite your live ~/.claude/settings.json — a .bak is written first)"
        );
    }
    println!("note: run `weave setup` to re-register the MCP server.");
    Ok(())
}

/// Read a file's bytes if it exists; `Ok(None)` when absent. A read error on an
/// existing file is propagated (not silently dropped).
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::from(e)).with_context(|| format!("reading {}", path.display()))
        }
    }
}

/// Copy `path` to `path.bak` before overwriting it (mirrors `setup.rs` `.bak`
/// discipline). Best-effort context-wrapped.
fn backup_existing(path: &Path) -> Result<()> {
    let bak = with_extra_extension(path, "bak");
    std::fs::copy(path, &bak)
        .with_context(|| format!("backing up {} -> {}", path.display(), bak.display()))?;
    Ok(())
}

/// `path` + `.<suffix>` (e.g. `foo.json` -> `foo.json.bak`).
fn with_extra_extension(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// A sibling temp path of `base` carrying `tag` + this process's pid, so concurrent
/// backups/restores do not collide on the staging file.
fn sibling_tmp(base: &Path, tag: &str) -> std::path::PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{tag}.{}.tmp", std::process::id()))
}

/// A small text MANIFEST recording weave version, backend, and which optional
/// members are present, so a restore can warn on a partial archive.
fn build_manifest(backend: &str, db_messages: i64, has_config: bool, has_settings: bool) -> String {
    format!(
        "weave-backup 1\nversion={}\nbackend={}\nmessages={}\n{}={}\n{}={}\n{}=present\n",
        env!("CARGO_PKG_VERSION"),
        backend,
        db_messages,
        ENTRY_CONFIG,
        if has_config { "present" } else { "absent" },
        ENTRY_SETTINGS,
        if has_settings { "present" } else { "absent" },
        ENTRY_DB,
    )
}

// Keep the constant referenced so a future divergence between the writer's entry
// set and the guard's accept-list is a compile-time concern, not a silent gap.
#[allow(dead_code)]
const _ENTRY_NAMES_REFERENCED: &[&str] = KNOWN_ENTRY_NAMES;
