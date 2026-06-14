//! WL-040: canonical session export / import I/O orchestration.
//!
//! `weave session export --out X [--for id]` serializes one identity's logical
//! state — its messages (read via [`Store::history`]), its tracked asks (read via
//! [`Store::list_asks`], recorded for fidelity), and the mesh memory entries
//! ([`weave_core::memory`]) — into a canonical, schema-versioned JSON document
//! ([`weave_core::session`]). `weave session import --in X [--as id] [--dry-run]`
//! reads that document back into a *different* weave instance: messages are
//! re-inserted via [`Store::send`] (free id-remap + idempotent dedup on
//! idempotency_key) and memory entries via `memory::memory_write`.
//!
//! This is the **logical, portable, versioned interchange** surface — distinct
//! from WL-034 (`weave export`, HTML presentation) and WL-035 (`weave backup`,
//! byte-exact host-local DB snapshot). See `docs/FORMAT-session-export.md`.
//!
//! Lives in the `weave` (bin) layer: all file + store + memory I/O is here; the
//! pure (de)serialize transforms live in `weave_core::session`. No upward dep, no
//! external program spawned (no-shell), all store writes via the parameterized
//! `Store::send`.
//!
//! ## Untrusted input
//!
//! An import file is **untrusted external input**. Every field is bounded BEFORE
//! it touches the store: `check_ident` on the importing identity and every
//! per-message sender/recipient, `check_body`/`MAX_BODY` on bodies, subject capped,
//! idempotency/trace ids shape-validated. The format embeds NO path fields, so a
//! crafted file cannot direct a write elsewhere; the only filesystem paths are the
//! user-named `--in`/`--out`, both UTF-8- and traversal-guarded.

use anyhow::{bail, Context, Result};
use std::path::Path;

use weave_core::config::Config;
use weave_core::memory::{self, MemoryScope};
use weave_core::model::{idempotency_key_valid, trace_id_valid};
use weave_core::session::{
    self, synth_idempotency_key, ExportedAsk, ExportedMemory, ExportedMessage, SessionExport,
};
use weave_core::store::{check_body, check_ident, Store, MAX_BODY};

/// Cap on an imported subject (the store `send` path does not bound `subject`
/// itself; mirror the body discipline so an untrusted file cannot smuggle an
/// unbounded subject). Generous — a real subject is short.
const MAX_IMPORT_SUBJECT: usize = 4_096;

// ===========================================================================
// Export
// ===========================================================================

/// `weave session export --out <out> [--for <id>] [--limit N] [--force]`.
///
/// Reads the identity's messages + asks + mesh memory, builds the canonical
/// envelope, and writes it atomically (sibling-temp + rename) with a read-back
/// verify of the message count (casr / WL-041 discipline).
pub fn run_export(
    cfg: &Config,
    store: &dyn Store,
    out: &Path,
    me: &str,
    limit: i64,
    force: bool,
) -> Result<()> {
    let _ = cfg;
    check_ident("identity", me)?;
    validate_out_path(out, force)?;

    // --- gather (all reads through existing Store / memory APIs) -----------
    let msgs = store
        .history(me, None, limit)
        .context("reading message history for export")?;
    let exported_msgs: Vec<ExportedMessage> = msgs
        .iter()
        .map(|m| ExportedMessage {
            id: m.id,
            ts: m.ts,
            sender: m.sender.clone(),
            recipient: m.recipient.clone(),
            subject: m.subject.clone(),
            body: m.body.clone(),
            idempotency_key: m.idempotency_key.clone(),
            trace_id: m.trace_id.clone(),
            priority: Some(m.priority.clone()),
        })
        .collect();

    let asks = store
        .list_asks(me, weave_core::model::AskRole::Any, limit)
        .context("reading tracked asks for export")?;
    let exported_asks: Vec<ExportedAsk> = asks
        .iter()
        .map(|a| ExportedAsk {
            id: a.id.clone(),
            question_msg_id: a.question_msg_id,
            answer_msg_id: a.answer_msg_id,
            asker: a.asker.clone(),
            askee: a.askee.clone(),
            subject: a.subject.clone(),
            state: a.state.as_str().to_string(),
            opened_ts: a.opened_ts,
            updated_ts: a.updated_ts,
            closed_ts: a.closed_ts,
        })
        .collect();

    let exported_mem = collect_memory().context("reading mesh memory for export")?;

    let envelope = session::serialize_session(
        me,
        weave_core::model::now(),
        exported_msgs,
        exported_asks,
        exported_mem,
    );
    let json = session::to_json(&envelope).context("serializing session export")?;

    // --- atomic write (sibling-temp + rename) ------------------------------
    let tmp = sibling_tmp(out, "weave-session");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("writing session export to {}", tmp.display()))?;
    std::fs::rename(&tmp, out)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), out.display()))?;

    // --- read-back verify (re-parse, assert message count) -----------------
    let written = std::fs::read_to_string(out)
        .with_context(|| format!("re-reading written export {}", out.display()))?;
    let parsed = session::from_json(&written).context("re-parsing the written export (verify)")?;
    if parsed.messages.len() != envelope.messages.len() {
        bail!(
            "session export verification failed: wrote {} message(s) but the file re-parses as {}",
            envelope.messages.len(),
            parsed.messages.len()
        );
    }

    println!(
        "session export written: {} ({} message(s), {} ask(s), {} memory entr{} for '{me}')",
        out.display(),
        envelope.messages.len(),
        envelope.asks.len(),
        envelope.memory.len(),
        if envelope.memory.len() == 1 {
            "y"
        } else {
            "ies"
        },
    );
    Ok(())
}

/// Read every mesh-memory entry across all scopes into the portable form.
fn collect_memory() -> Result<Vec<ExportedMemory>> {
    let mut out = Vec::new();
    for scope in memory::memory_scopes()? {
        let (kind, name) = scope_to_strings(&scope);
        for e in memory::memory_list(&scope)? {
            out.push(ExportedMemory {
                scope_kind: kind.clone(),
                scope_name: name.clone(),
                key: e.key,
                title: e.title,
                tags: e.tags,
                body: e.body,
            });
        }
    }
    Ok(out)
}

/// Map a [`MemoryScope`] to its `(kind, name)` string pair for the envelope.
fn scope_to_strings(scope: &MemoryScope) -> (String, String) {
    match scope {
        MemoryScope::Global => ("global".to_string(), String::new()),
        MemoryScope::Project(p) => ("project".to_string(), p.clone()),
        MemoryScope::Persona(p) => ("persona".to_string(), p.clone()),
        MemoryScope::Orchestrator(c) => ("orchestrator".to_string(), c.clone()),
    }
}

/// Reconstruct a [`MemoryScope`] from the envelope's `(scope_kind, scope_name)`.
/// An unknown kind is a hard error (the file is untrusted; we do not silently
/// coerce a scope we cannot model).
fn scope_from_strings(kind: &str, name: &str) -> Result<MemoryScope> {
    match kind {
        "global" => Ok(MemoryScope::Global),
        "project" => Ok(MemoryScope::Project(name.to_string())),
        "persona" => Ok(MemoryScope::Persona(name.to_string())),
        "orchestrator" => Ok(MemoryScope::Orchestrator(name.to_string())),
        other => bail!("unknown memory scope kind '{other}' in import file"),
    }
}

// ===========================================================================
// Import
// ===========================================================================

/// `weave session import --in <in_path> [--as <id>] [--dry-run]`.
///
/// Reads + validates the document, then (unless `--dry-run`) re-inserts messages
/// via [`Store::send`] under the importing identity (id-remap is automatic; dedup
/// is by idempotency_key, synthesized for keyless legacy messages) and writes
/// memory entries via `memory::memory_write`. Idempotent on re-run. Asks are
/// recorded in the file but NOT replayed in this version (see WL-040b).
pub fn run_import(
    cfg: &Config,
    store: &dyn Store,
    in_path: &Path,
    as_id: &str,
    dry_run: bool,
) -> Result<()> {
    let _ = cfg;
    check_ident("identity", as_id)?;
    validate_in_path(in_path)?;

    let raw = std::fs::read_to_string(in_path)
        .with_context(|| format!("reading session import file {}", in_path.display()))?;
    if raw.len() > MAX_IMPORT_FILE_BYTES {
        bail!(
            "session import file is too large ({} bytes; max {MAX_IMPORT_FILE_BYTES})",
            raw.len()
        );
    }
    let envelope: SessionExport =
        session::from_json(&raw).context("parsing the session import file")?;

    // --- validate EVERY field BEFORE any store write (untrusted input) -----
    validate_messages(&envelope.messages)?;
    validate_memory(&envelope.memory)?;

    let mut msg_inserted = 0usize;
    let mut msg_skipped = 0usize;
    let mut mem_written = 0usize;

    if !dry_run {
        for m in &envelope.messages {
            // Identity remap: rewrite occurrences of the SOURCE identity to the
            // importing identity, preserving third-party names. This resumes the
            // session under the new name.
            let sender = remap(&m.sender, &envelope.identity, as_id);
            let recipient = remap(&m.recipient, &envelope.identity, as_id);
            // Dedup key: the source key if present, else a deterministic synth key
            // so re-import of a keyless legacy message is still idempotent.
            let key = match &m.idempotency_key {
                Some(k) => k.clone(),
                None => synth_idempotency_key(&envelope.identity, m.id),
            };
            let before = store.total_messages().unwrap_or(-1);
            let _id = store
                .send(
                    &sender,
                    &recipient,
                    m.subject.as_deref(),
                    &m.body,
                    Some(&key),
                    m.trace_id.as_deref(),
                )
                .with_context(|| format!("importing message id {} from source", m.id))?;
            let after = store.total_messages().unwrap_or(-1);
            if after > before {
                msg_inserted += 1;
            } else {
                msg_skipped += 1;
            }
        }

        for e in &envelope.memory {
            let scope = scope_from_strings(&e.scope_kind, &e.scope_name)?;
            // memory_write re-bounds key/title/tags/body and preserves created_ts
            // on an existing file (idempotent overwrite).
            memory::memory_write(&scope, &e.key, &e.title, &e.tags, &e.body)
                .with_context(|| format!("importing memory entry '{}'", e.key))?;
            mem_written += 1;
        }
    }

    let ask_note = if envelope.asks.is_empty() {
        String::new()
    } else {
        format!(
            "; {} ask(s) in archive not imported — see WL-040b",
            envelope.asks.len()
        )
    };

    if dry_run {
        println!(
            "session import (dry-run): {} message(s), {} memory entr{} would be imported as \
             '{as_id}'{ask_note} (no changes written)",
            envelope.messages.len(),
            envelope.memory.len(),
            if envelope.memory.len() == 1 {
                "y"
            } else {
                "ies"
            },
        );
    } else {
        println!(
            "session import complete: {} message(s) inserted, {} skipped (already present); \
             {} memory entr{} written as '{as_id}'{ask_note}",
            msg_inserted,
            msg_skipped,
            mem_written,
            if mem_written == 1 { "y" } else { "ies" },
        );
    }
    Ok(())
}

/// Rewrite `name` from the source identity to the importing identity, preserving
/// every third-party name verbatim. When `--as` equals the source identity this is
/// an identity-preserving import (the common cross-machine resume case).
fn remap(name: &str, source_identity: &str, importing: &str) -> String {
    if name == source_identity {
        importing.to_string()
    } else {
        name.to_string()
    }
}

/// Bound every message field before any store write (the central import invariant).
fn validate_messages(msgs: &[ExportedMessage]) -> Result<()> {
    for m in msgs {
        check_ident("message sender", &m.sender)?;
        check_ident("message recipient", &m.recipient)?;
        check_body(&m.body)?;
        if let Some(s) = &m.subject {
            if s.len() > MAX_IMPORT_SUBJECT {
                bail!(
                    "imported subject is too long ({} bytes; max {MAX_IMPORT_SUBJECT})",
                    s.len()
                );
            }
        }
        if let Some(k) = &m.idempotency_key {
            if !idempotency_key_valid(k) {
                bail!("imported message carries an invalid idempotency_key");
            }
        }
        if let Some(t) = &m.trace_id {
            if !trace_id_valid(t) {
                bail!("imported message carries an invalid trace_id");
            }
        }
    }
    Ok(())
}

/// Bound every memory field before any filesystem write. `memory_write` re-bounds
/// internally; this is defense-in-depth + a clear early error on a hostile file.
fn validate_memory(mem: &[ExportedMemory]) -> Result<()> {
    for e in mem {
        // scope_kind must be one we model.
        scope_from_strings(&e.scope_kind, &e.scope_name)?;
        if e.body.len() > MAX_BODY {
            bail!(
                "imported memory body is too long ({} bytes; max {MAX_BODY})",
                e.body.len()
            );
        }
    }
    Ok(())
}

// ===========================================================================
// Path guards (mirror backup.rs)
// ===========================================================================

/// Hard upper bound on an import file size. An import file is untrusted; an
/// unbounded one is a RAM DoS once read into memory + parsed. Generous: a session
/// of 10k messages at MAX_BODY each is well under this in practice, and the
/// per-field caps still apply.
const MAX_IMPORT_FILE_BYTES: usize = 256 * 1024 * 1024;

/// UTF-8 / overwrite / parent-exists guard on `--out` (copy of `backup.rs`).
fn validate_out_path(out: &Path, force: bool) -> Result<()> {
    let out_str = out.as_os_str().to_str();
    if out_str.is_none_or(str::is_empty) {
        bail!("session export --out path is empty or not valid UTF-8");
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
                "session export --out parent directory does not exist: {}",
                parent.display()
            );
        }
    }
    Ok(())
}

/// UTF-8 / existence guard on `--in`. The format carries NO embedded path fields,
/// so there is no in-payload traversal vector — only the user-named path is read.
fn validate_in_path(in_path: &Path) -> Result<()> {
    let in_str = in_path.as_os_str().to_str();
    if in_str.is_none_or(str::is_empty) {
        bail!("session import --in path is empty or not valid UTF-8");
    }
    if !in_path.exists() {
        bail!(
            "session import --in file does not exist: {}",
            in_path.display()
        );
    }
    if in_path.is_dir() {
        bail!(
            "session import --in is a directory, expected a file: {}",
            in_path.display()
        );
    }
    Ok(())
}

/// A sibling temp path of `base` carrying `tag` + this process's pid (mirror of
/// `backup.rs::sibling_tmp`).
fn sibling_tmp(base: &Path, tag: &str) -> std::path::PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{tag}.{}.tmp", std::process::id()))
}
