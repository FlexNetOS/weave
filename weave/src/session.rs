//! WL-040: canonical session export / import I/O orchestration.
//!
//! `weave session export --out X [--for id]` serializes one identity's logical
//! state — its messages (read via [`Store::history`]), its tracked asks (read via
//! [`Store::list_asks`], recorded for fidelity), and the mesh memory entries
//! ([`weave_core::memory`]) — into a canonical, schema-versioned JSON document
//! ([`weave_core::session`]). `weave session import --in X [--as id] [--dry-run]`
//! reads that document back into a *different* weave instance: messages are
//! re-inserted through atomic keyed plain/configured/reply seams (free id-remap +
//! exact operation-aware dedup on idempotency_key), tracked asks are faithfully
//! replayed via `Store::import_ask`
//! / `Store::import_ask_group` (WL-040b — message links remapped to the freshly
//! minted local ids, materialized in their exported `AskState`), and memory
//! entries via `memory::memory_write`.
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
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use std::collections::{HashMap, HashSet};

use weave_core::config::Config;
use weave_core::memory::{self, MemoryScope};
use weave_core::model::{
    ask_id_valid, ask_many_id_valid, idempotency_key_valid, trace_id_valid, AskKind, AskState,
    KIND_SESSION_PLAIN,
};
use weave_core::session::{
    self, synth_idempotency_key, ExportedAsk, ExportedAskGroup, ExportedConfiguredSend,
    ExportedMemory, ExportedMessage, SessionExport, MAX_SESSION_ASKS, MAX_SESSION_ASK_GROUPS,
    MAX_SESSION_MEMORY_ENTRIES, MAX_SESSION_MESSAGES,
};
use weave_core::store::{
    check_body, check_ident, check_subject, ImportedAskSourceTimestamps, Store, MAX_BODY,
};

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
    let asks = store
        .list_asks(me, weave_core::model::AskRole::Any, limit)
        .context("reading tracked asks for export")?;
    let mut exported_asks: Vec<ExportedAsk> = asks
        .iter()
        .map(|a| ExportedAsk {
            id: a.id.clone(),
            question_msg_id: a.question_msg_id,
            answer_msg_id: a.answer_msg_id,
            asker: a.asker.clone(),
            askee: a.askee.clone(),
            subject: a.subject.clone(),
            state: a.state.as_str().to_string(),
            kind: a.kind.as_str().to_string(),
            options: a.options.clone(),
            reply_to: a.reply_to.clone(),
            close_note: a.close_note.clone(),
            opened_ts: a.opened_ts,
            updated_ts: a.updated_ts,
            closed_ts: a.closed_ts,
            parent_id: a.parent_id.clone(),
        })
        .collect();

    // Ask question/answer rows are rehydrated through `import_ask`, replies need
    // their parent semantics, and a schema-v1/plain restore has no original
    // request tuple. Only top-level configured sends carry `configured_send`.
    let mut ask_message_ids = HashSet::new();
    for ask in &asks {
        ask_message_ids.insert(ask.question_msg_id);
        if let Some(id) = ask.answer_msg_id {
            ask_message_ids.insert(id);
        }
    }
    let mut exported_msgs: Vec<ExportedMessage> = msgs
        .iter()
        .map(|m| {
            let configured_send = if !ask_message_ids.contains(&m.id)
                && m.in_reply_to.is_none()
                && m.kind.as_deref() != Some(KIND_SESSION_PLAIN)
            {
                match (
                    m.request_priority.as_ref(),
                    m.request_ttl,
                    m.request_supersedes,
                    m.request_dedup_idle,
                ) {
                    (Some(priority), Some(ttl), Some(supersedes), Some(dedup_idle)) => {
                        Some(ExportedConfiguredSend {
                            priority: priority.clone(),
                            ttl,
                            supersedes: (supersedes > 0).then_some(supersedes),
                            dedup_idle,
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };
            ExportedMessage {
                id: m.id,
                ts: m.ts,
                sender: m.sender.clone(),
                recipient: m.recipient.clone(),
                subject: m.subject.clone(),
                body: m.body.clone(),
                in_reply_to: m.in_reply_to,
                reply_ttl: if m.in_reply_to.is_some() {
                    m.request_ttl
                        .or_else(|| {
                            m.expires_at
                                .map(|expires_at| expires_at.saturating_sub(m.ts).max(0))
                        })
                        .unwrap_or(0)
                } else {
                    0
                },
                idempotency_key: m.idempotency_key.clone(),
                trace_id: m.trace_id.clone(),
                priority: Some(m.priority.clone()),
                superseded_by: m.superseded_by,
                configured_send,
            }
        })
        .collect();
    let exported_ids: HashSet<i64> = exported_msgs.iter().map(|message| message.id).collect();
    // A bounded message snapshot must close over every live ask message. Metadata
    // left behind by an older retention pass is different: skip that already-
    // dangling ask rather than making all future exports unusable.
    let mut closed_asks = Vec::with_capacity(exported_asks.len());
    let mut retention_dangling_ask_ids = HashSet::new();
    for ask in exported_asks {
        let mut retained = true;
        for (role, required_id) in std::iter::once(("question", ask.question_msg_id))
            .chain(ask.answer_msg_id.map(|id| ("answer", id)))
        {
            if !exported_ids.contains(&required_id) {
                if store.message_exists(required_id)? {
                    bail!(
                        "session export limit omitted {role} message {required_id} required by ask '{}'",
                        ask.id
                    );
                }
                retained = false;
                break;
            }
        }
        if retained {
            closed_asks.push(ask);
        } else {
            retention_dangling_ask_ids.insert(ask.id);
        }
    }
    exported_asks = closed_asks;

    // Ask-chain links are portable too. An existing source ask omitted by the ask
    // limit is a closure error; a genuinely removed metadata target is normalized
    // to an unchained ask, analogous to a retained reply whose parent was deleted.
    let exported_ask_ids: HashSet<String> =
        exported_asks.iter().map(|ask| ask.id.clone()).collect();
    for ask in &mut exported_asks {
        if let Some(reply_to) = ask.reply_to.clone() {
            if !exported_ask_ids.contains(&reply_to) {
                if retention_dangling_ask_ids.contains(&reply_to) {
                    // The source still has the parent's ask metadata, but its
                    // required message was already removed by retention. There is
                    // no reconstructable parent to export, so preserve the child as
                    // a standalone ask instead of misreporting an ask-limit error.
                    ask.reply_to = None;
                } else {
                    match store.get_ask(&reply_to)? {
                        Some(parent) => {
                            let question_survives = store.message_exists(parent.question_msg_id)?;
                            let answer_survives = match parent.answer_msg_id {
                                Some(answer_id) => store.message_exists(answer_id)?,
                                None => true,
                            };
                            if question_survives && answer_survives {
                                bail!(
                                    "session export limit omitted ask '{reply_to}' required by chained ask '{}'",
                                    ask.id
                                );
                            }
                            // Legacy/partially-retained stores can keep parent ask
                            // metadata after one of its required messages is gone.
                            // Such a parent is inherently unreconstructable, not a
                            // live row hidden only by --limit.
                            ask.reply_to = None;
                        }
                        None => ask.reply_to = None,
                    }
                }
            }
        }
    }
    for message in &mut exported_msgs {
        if let Some(parent) = message.in_reply_to {
            if !exported_ids.contains(&parent) {
                if store.message_exists(parent)? {
                    bail!(
                        "session export limit omitted parent message {parent} required by reply {}",
                        message.id
                    );
                }
                // Retention may remove a reply's source-local parent while the
                // child remains useful. No target id can represent that deleted
                // parent, so port the surviving row as an explicit top-level send
                // while retaining its content, priority, and relative TTL.
                let ttl = message.reply_ttl;
                message.in_reply_to = None;
                message.reply_ttl = 0;
                message.configured_send = Some(ExportedConfiguredSend {
                    priority: message
                        .priority
                        .clone()
                        .unwrap_or_else(|| "normal".to_string()),
                    ttl,
                    supersedes: None,
                    dedup_idle: false,
                });
            }
        }
        if let Some(predecessor) = message
            .configured_send
            .as_ref()
            .and_then(|configured| configured.supersedes)
        {
            if !exported_ids.contains(&predecessor) {
                if store.message_exists(predecessor)? {
                    bail!(
                        "session export limit omitted predecessor message {predecessor} required by {}",
                        message.id
                    );
                }
                // TTL/retention may legitimately remove the predecessor after an
                // accepted configured send. The source Store retains its private
                // request id for exact local replay, but that dead local row id has
                // no portable target mapping and is omitted from the snapshot.
                if let Some(configured) = &mut message.configured_send {
                    configured.supersedes = None;
                }
            }
        }
        if let Some(successor) = message.superseded_by {
            if !exported_ids.contains(&successor) {
                if store.message_exists(successor)? {
                    bail!(
                        "session export limit omitted successor message {successor} referenced by {}",
                        message.id
                    );
                }
                message.superseded_by = None;
            }
        }
    }

    // WL-040b: carry the ask-many PARENT anchor rows the exported child asks
    // reference, so `parent_id` linkage + group totality (`target_count`) survive a
    // round-trip. Gather the distinct parent ids, then read the group rows.
    let mut group_ids: Vec<String> = Vec::new();
    for a in &exported_asks {
        if let Some(p) = &a.parent_id {
            if !group_ids.iter().any(|g| g == p) {
                group_ids.push(p.clone());
            }
        }
    }
    let selected_ask_ids: HashSet<&str> = exported_asks.iter().map(|ask| ask.id.as_str()).collect();
    for group_id in &group_ids {
        let source_children = store
            .list_ask_group_children(group_id)
            .with_context(|| format!("reading children for ask group '{group_id}'"))?;
        if source_children.len() > weave_core::store::MAX_ASK_MANY_TARGETS {
            bail!("ask group '{group_id}' exceeds the supported child bound");
        }
        for child in source_children {
            if selected_ask_ids.contains(child.id.as_str()) {
                continue;
            }
            let question_survives = store.message_exists(child.question_msg_id)?;
            let answer_survives = match child.answer_msg_id {
                Some(answer_id) => store.message_exists(answer_id)?,
                None => true,
            };
            if question_survives && answer_survives {
                bail!(
                    "session export limit omitted ask '{}' required by ask group '{group_id}'",
                    child.id
                );
            }
        }
    }
    let exported_groups: Vec<ExportedAskGroup> = store
        .list_ask_groups(&group_ids)
        .context("reading ask-many groups for export")?
        .into_iter()
        .map(|g| ExportedAskGroup {
            parent_id: g.parent_id,
            asker: g.asker,
            subject: g.subject,
            body: g.body,
            opened_ts: g.opened_ts,
            target_count: g.target_count,
        })
        .collect();

    let exported_mem = collect_memory().context("reading mesh memory for export")?;

    let envelope = session::serialize_session(
        me,
        weave_core::model::now(),
        exported_msgs,
        exported_asks,
        exported_groups,
        exported_mem,
    );
    validate_envelope_counts(&envelope)?;
    let json = session::to_json(&envelope).context("serializing session export")?;
    validate_export_size(json.len())?;

    // --- private atomic write (exclusive sibling-temp + publish) -----------
    write_export_atomically(out, json.as_bytes(), force)?;

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
        "session export written: {} ({} message(s), {} ask(s), {} ask group(s), {} memory \
         entr{} for '{me}')",
        out.display(),
        envelope.messages.len(),
        envelope.asks.len(),
        envelope.ask_groups.len(),
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
        "global" if name.is_empty() => Ok(MemoryScope::Global),
        "global" => bail!("global memory scope must carry an empty scope_name"),
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
/// is by idempotency_key, synthesized for keyless legacy messages), replays the
/// tracked asks + ask-many groups (WL-040b: each ask materialized in its exported
/// `AskState` with its message links remapped; a dangling ask — one whose message
/// is absent from the export — is skipped, never linked broken), and writes memory
/// entries via `memory::memory_write`. Idempotent on re-run.
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

    let raw = read_import_bounded(in_path)?;
    let envelope: SessionExport =
        session::from_json(&raw).context("parsing the session import file")?;

    // --- validate EVERY field BEFORE any store write (untrusted input) -----
    check_ident("source identity", &envelope.identity)?;
    validate_envelope_counts(&envelope)?;
    validate_messages(&envelope.messages, &envelope.identity, &envelope.asks)?;
    validate_ask_groups(&envelope.ask_groups)?;
    validate_asks(&envelope.asks)?;
    validate_identity_remap(&envelope, as_id)?;
    validate_ask_relations(&envelope.messages, &envelope.asks, &envelope.ask_groups)?;
    validate_memory(&envelope.memory)?;

    let source_message_ids: HashSet<i64> =
        envelope.messages.iter().map(|message| message.id).collect();
    let source_message_timestamps: HashMap<i64, i64> = envelope
        .messages
        .iter()
        .map(|message| (message.id, message.ts))
        .collect();
    let reconstructable_asks = reconstructable_asks(&envelope.asks, &source_message_ids);

    let mut msg_inserted = 0usize;
    let mut msg_skipped = 0usize;
    let mut mem_written = 0usize;
    // WL-040b counters: asks/groups actually replayed (newly inserted), skipped as
    // already-present (idempotent re-import), and dangling (an ask whose question/
    // answer message was not in the export — skipped, never a broken link).
    let mut ask_replayed = 0usize;
    let mut ask_skipped = 0usize;
    let ask_dangling: usize;
    let mut group_replayed = 0usize;
    let mut group_skipped = 0usize;
    // Count of asks that WOULD replay in dry-run (including reply-chain ancestry).
    let ask_would_replay = reconstructable_asks.len();

    if !dry_run {
        // WL-040b: map each SOURCE message id to its REMAPPED local id so asks can
        // rewire question/answer links to the freshly re-minted rows. Re-import
        // first performs an exact key lookup because the already-imported message
        // may now be attached to an ask and is no longer a plain-send replay.
        let mut msg_id_map: HashMap<i64, i64> = HashMap::with_capacity(envelope.messages.len());
        let source_ask_message_ids: HashSet<i64> = envelope
            .asks
            .iter()
            .flat_map(|ask| std::iter::once(ask.question_msg_id).chain(ask.answer_msg_id))
            .collect();
        let mut ordered_messages: Vec<&ExportedMessage> = envelope.messages.iter().collect();
        ordered_messages.sort_by_key(|message| message.id);
        for m in ordered_messages {
            // Identity remap: rewrite occurrences of the SOURCE identity to the
            // importing identity, preserving third-party names. This resumes the
            // session under the new name.
            let sender = remap(&m.sender, &envelope.identity, as_id);
            let recipient = remap(&m.recipient, &envelope.identity, as_id);
            // Dedup key: the source key if present, else a deterministic synth key
            // so re-import of a keyless legacy message is still idempotent.
            let synthesized_key = m.idempotency_key.is_none();
            let key = m
                .idempotency_key
                .clone()
                .unwrap_or_else(|| synth_idempotency_key(&envelope.identity, m.id));
            let effective_priority = m.priority.as_deref().unwrap_or("normal");
            let existing = store.message_by_idempotency_key(&key)?;
            if synthesized_key
                && existing.as_ref().is_some_and(|existing| {
                    existing.sender != sender
                        || existing.recipient != recipient
                        || existing.subject != m.subject
                        || existing.body != m.body
                })
            {
                bail!(
                    "importing keyless message id {}: source namespace conflict for identity '{}'; \
                     import independent same-identity stores into separate targets or add stable source keys",
                    m.id,
                    envelope.identity
                );
            }
            let (new_id, created) = if let Some(source_parent) = m.in_reply_to {
                let parent = *msg_id_map.get(&source_parent).ok_or_else(|| {
                    anyhow::anyhow!(
                        "importing reply id {}: parent {source_parent} was not remapped",
                        m.id
                    )
                })?;
                match existing.as_ref() {
                    Some(existing)
                        if existing.sender == sender
                            && existing.recipient == recipient
                            && existing.subject == m.subject
                            && existing.body == m.body
                            && existing.in_reply_to == Some(parent)
                            && existing.priority == effective_priority
                            && existing.request_priority.as_deref() == Some(effective_priority)
                            && existing.request_ttl == Some(m.reply_ttl) =>
                    {
                        (existing.id, false)
                    }
                    Some(_) => bail!(
                        "importing reply id {}: idempotency key belongs to different content",
                        m.id
                    ),
                    None => {
                        let (id, created) = store
                            .reply_configured_idempotent(
                                &sender,
                                parent,
                                &m.body,
                                Some(&key),
                                Some(effective_priority),
                                m.reply_ttl,
                                m.subject.as_deref(),
                            )
                            .with_context(|| format!("importing reply message id {}", m.id))?;
                        (id, created)
                    }
                }
            } else if let Some(configured) = &m.configured_send {
                if let Some(existing) = existing.as_ref() {
                    if existing.priority != effective_priority {
                        bail!(
                            "importing message id {}: existing key has different effective priority",
                            m.id
                        );
                    }
                }
                let supersedes = match configured.supersedes {
                    Some(source_id) => Some(*msg_id_map.get(&source_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "importing message id {}: predecessor {source_id} was not remapped",
                            m.id
                        )
                    })?),
                    None => None,
                };
                let (id, created) = store
                    .send_configured_idempotent_mode(
                        &sender,
                        &recipient,
                        m.subject.as_deref(),
                        &m.body,
                        Some(&key),
                        m.trace_id.as_deref(),
                        Some(&configured.priority),
                        Some(effective_priority),
                        supersedes,
                        configured.ttl,
                        configured.dedup_idle,
                        true,
                        false,
                    )
                    .with_context(|| format!("importing configured message id {}", m.id))?;
                (id, created)
            } else {
                // Ask/answer rows can already be attached to their tracked record
                // on re-import. Recognize them by exact message content; trace is
                // attempt-local and the first accepted trace remains authoritative.
                let existing_is_tracked = match existing.as_ref() {
                    Some(existing) => store.ask_for_message(existing.id)?.is_some(),
                    None => false,
                };
                let portable_plain_marker = existing.as_ref().is_some_and(|existing| {
                    existing.kind.as_deref() == Some(KIND_SESSION_PLAIN)
                        && existing.request_priority.as_deref() == Some(effective_priority)
                        && existing.request_ttl == Some(0)
                        && existing.request_supersedes == Some(0)
                        && existing.request_dedup_idle == Some(false)
                });
                let legacy_tracked_default = existing.as_ref().is_some_and(|existing| {
                    source_ask_message_ids.contains(&m.id)
                        && existing_is_tracked
                        && existing.kind.is_none()
                        && existing.request_priority.as_deref() == Some("normal")
                        && existing.request_ttl == Some(0)
                        && existing.request_supersedes == Some(0)
                        && existing.request_dedup_idle == Some(false)
                });
                match existing.as_ref() {
                    Some(existing)
                        if existing.sender == sender
                            && existing.recipient == recipient
                            && existing.subject == m.subject
                            && existing.body == m.body
                            && existing.in_reply_to.is_none()
                            && existing.expires_at.is_none()
                            && existing.priority == effective_priority
                            && (portable_plain_marker || legacy_tracked_default) =>
                    {
                        (existing.id, false)
                    }
                    Some(_) => bail!(
                        "importing message id {}: idempotency key belongs to different content",
                        m.id
                    ),
                    None => {
                        let (id, created) = store
                            .send_imported_idempotent(
                                &sender,
                                &recipient,
                                m.subject.as_deref(),
                                &m.body,
                                Some(&key),
                                m.trace_id.as_deref(),
                                effective_priority,
                            )
                            .with_context(|| {
                                format!("importing message id {} from source", m.id)
                            })?;
                        (id, created)
                    }
                }
            };
            if created {
                msg_inserted += 1;
            } else {
                msg_skipped += 1;
            }
            msg_id_map.insert(m.id, new_id);
        }

        // Restore only source-exported effective successor links. This avoids
        // replaying dedup-idle as a broad target-local sweep while preserving the
        // source snapshot and the exact future retry tuple.
        for m in &envelope.messages {
            let Some(source_successor) = m.superseded_by else {
                continue;
            };
            let old_id = msg_id_map[&m.id];
            let new_id = msg_id_map[&source_successor];
            let sender = remap(&m.sender, &envelope.identity, as_id);
            store
                .supersede(&sender, old_id, new_id)
                .with_context(|| format!("restoring successor link for message id {}", m.id))?;
        }

        // WL-040b: replay ask-many PARENT anchors BEFORE the child asks so each
        // child's parent_id resolves to the replayed group. Source group ids are
        // instance-scoped, so mint a fresh local id per group and remember the
        // source→new mapping for the children.
        let mut group_id_map: HashMap<String, String> =
            HashMap::with_capacity(envelope.ask_groups.len());
        for g in &envelope.ask_groups {
            let asker = remap(&g.asker, &envelope.identity, as_id);
            let new_parent = imported_group_id(&envelope.identity, &g.parent_id);
            let inserted = store
                .import_ask_group(
                    &new_parent,
                    &asker,
                    g.subject.as_deref(),
                    &g.body,
                    g.opened_ts,
                    g.target_count,
                )
                .with_context(|| format!("replaying ask group '{}'", g.parent_id))?;
            group_id_map.insert(g.parent_id.clone(), new_parent);
            if inserted {
                group_replayed += 1;
            } else {
                group_skipped += 1;
            }
        }

        // Resolve every reconstructable source ask to its stable target id before
        // inserting any ask. This makes reply_to chains order-independent and also
        // adopts the actual id from an earlier partial/repeated import.
        ask_dangling = envelope.asks.len() - reconstructable_asks.len();
        let mut ask_id_map: HashMap<String, String> =
            HashMap::with_capacity(reconstructable_asks.len());
        for ask in &reconstructable_asks {
            let question_id = msg_id_map[&ask.question_msg_id];
            let target_id = store
                .ask_for_message(question_id)?
                .unwrap_or_else(|| imported_ask_id(&envelope.identity, &ask.id));
            ask_id_map.insert(ask.id.clone(), target_id);
        }

        // WL-040b: replay each ask thread. Resolve the remapped question/answer
        // message ids; an ask referencing a message NOT in the export is dangling and
        // skipped (never inserts a broken link). The ask id is regenerated from the
        // remapped question id (the source id is meaningless here); dedup on
        // (asker, askee, question_msg_id) keeps re-import idempotent.
        for a in reconstructable_asks {
            let new_id = &ask_id_map[&a.id];
            let new_q = msg_id_map[&a.question_msg_id];
            let new_a = a.answer_msg_id.map(|src_a| msg_id_map[&src_a]);
            let asker = remap(&a.asker, &envelope.identity, as_id);
            let askee = remap(&a.askee, &envelope.identity, as_id);
            let state = AskState::from_str(&a.state)
                .map_err(|m| anyhow::anyhow!("imported ask carries an invalid state: {m}"))?;
            let kind = AskKind::parse(&a.kind);
            // Rewire parent_id to the replayed group; a parent we did not replay (it
            // was absent from the export) is dropped to NULL so the ask still replays
            // standalone rather than dangling on a missing group.
            let new_parent = a.parent_id.as_ref().and_then(|p| group_id_map.get(p));
            let new_reply_to = a
                .reply_to
                .as_ref()
                .map(|source_id| ask_id_map[source_id].as_str());
            let inserted = store
                .import_ask_with_source_timestamps(
                    new_id,
                    new_q,
                    new_a,
                    &asker,
                    &askee,
                    a.subject.as_deref(),
                    state,
                    kind,
                    a.options.as_deref(),
                    new_reply_to,
                    a.close_note.as_deref(),
                    ImportedAskSourceTimestamps {
                        question: source_message_timestamps[&a.question_msg_id],
                        answer: a
                            .answer_msg_id
                            .map(|answer_id| source_message_timestamps[&answer_id]),
                    },
                    a.opened_ts,
                    a.updated_ts,
                    a.closed_ts,
                    new_parent.map(|s| s.as_str()),
                )
                .with_context(|| format!("replaying ask '{}'", a.id))?;
            if inserted {
                ask_replayed += 1;
            } else {
                ask_skipped += 1;
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
    } else {
        // Match the real importer exactly: a child is reconstructable only when
        // its own messages AND its whole reply_to ancestry are reconstructable.
        ask_dangling = envelope.asks.len() - ask_would_replay;
    }

    if dry_run {
        let dangling_note = if ask_dangling > 0 {
            format!(" ({ask_dangling} dangling ask(s) would be skipped)")
        } else {
            String::new()
        };
        println!(
            "session import (dry-run): {} message(s), {} ask(s), {} ask group(s), {} memory \
             entr{} would be imported as '{as_id}'{dangling_note} (no changes written)",
            envelope.messages.len(),
            ask_would_replay,
            envelope.ask_groups.len(),
            envelope.memory.len(),
            if envelope.memory.len() == 1 {
                "y"
            } else {
                "ies"
            },
        );
    } else {
        let dangling_note = if ask_dangling > 0 {
            format!(", {ask_dangling} dangling skipped")
        } else {
            String::new()
        };
        println!(
            "session import complete: {} message(s) inserted, {} skipped (already present); \
             {} ask(s) replayed, {} ask(s) skipped (already present){dangling_note}; \
             {} ask group(s) replayed, {} skipped; \
             {} memory entr{} written as '{as_id}'",
            msg_inserted,
            msg_skipped,
            ask_replayed,
            ask_skipped,
            group_replayed,
            group_skipped,
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

/// Stable target id for an imported ask-many parent. Two independently seeded
/// FNV-1a lanes keep the bounded identifier deterministic across processes and
/// clocks; the Store verifies exact payload equality on an existing id.
fn imported_group_id(source_identity: &str, source_parent_id: &str) -> String {
    fn lane(seed: u64, parts: [&[u8]; 2]) -> u64 {
        let mut hash = seed;
        for part in parts {
            for byte in part {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
    let parts = [source_identity.as_bytes(), source_parent_id.as_bytes()];
    let first = lane(0xcbf29ce484222325, parts);
    let second = lane(0x84222325cbf29ce4, parts);
    format!("askm_imp_{first:016x}{second:016x}")
}

/// Stable target id for an imported tracked ask. The source correlation id is
/// instance-local, but it is unique within the document and safe input to the same
/// two-lane bounded hash used for imported ask groups. Stability is required so a
/// reply_to chain can be mapped before either endpoint is inserted.
fn imported_ask_id(source_identity: &str, source_ask_id: &str) -> String {
    fn lane(seed: u64, parts: [&[u8]; 2]) -> u64 {
        let mut hash = seed;
        for part in parts {
            for byte in part {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
    let parts = [source_identity.as_bytes(), source_ask_id.as_bytes()];
    let first = lane(0x517cc1b727220a95, parts);
    let second = lane(0x9e3779b185ebca87, parts);
    format!("ask_imp_{first:016x}{second:016x}")
}

/// Return asks in parent-before-child order whose entire portable dependency
/// closure is present. A missing question/answer makes that ask unreconstructable;
/// every chained descendant is likewise omitted so import can never materialize a
/// `reply_to` pointing at a row that was skipped. `validate_asks` has already
/// rejected missing ids and cycles, so fixed-point exhaustion is deterministic.
fn reconstructable_asks<'a>(
    asks: &'a [ExportedAsk],
    present_messages: &HashSet<i64>,
) -> Vec<&'a ExportedAsk> {
    let mut resolved = HashSet::with_capacity(asks.len());
    let mut ordered = Vec::with_capacity(asks.len());
    loop {
        let before = ordered.len();
        for ask in asks {
            if resolved.contains(ask.id.as_str())
                || !present_messages.contains(&ask.question_msg_id)
                || ask
                    .answer_msg_id
                    .is_some_and(|id| !present_messages.contains(&id))
                || ask
                    .reply_to
                    .as_deref()
                    .is_some_and(|parent| !resolved.contains(parent))
            {
                continue;
            }
            resolved.insert(ask.id.as_str());
            ordered.push(ask);
        }
        if ordered.len() == before {
            return ordered;
        }
    }
}

/// Keep programmatically constructed exports subject to the same collection
/// bounds enforced by the parser. This makes every emitted envelope importable by
/// the same build and prevents later validation from allocating oversized maps.
fn validate_envelope_counts(envelope: &SessionExport) -> Result<()> {
    for (field, actual, limit) in [
        ("messages", envelope.messages.len(), MAX_SESSION_MESSAGES),
        ("asks", envelope.asks.len(), MAX_SESSION_ASKS),
        (
            "ask groups",
            envelope.ask_groups.len(),
            MAX_SESSION_ASK_GROUPS,
        ),
        (
            "memory entries",
            envelope.memory.len(),
            MAX_SESSION_MEMORY_ENTRIES,
        ),
    ] {
        if actual > limit {
            bail!("session {field} exceeds {limit} entries");
        }
    }
    Ok(())
}

/// `--as` replaces every occurrence of the source identity. If the requested
/// target name already denotes a distinct actor in the envelope, that rewrite
/// would collapse two principals (for example A→B becoming B→B). Reject the
/// ambiguous mapping before any message, ask, group, or memory write.
fn validate_identity_remap(envelope: &SessionExport, as_id: &str) -> Result<()> {
    if as_id == envelope.identity {
        return Ok(());
    }
    let conflicts = envelope
        .messages
        .iter()
        .any(|message| message.sender == as_id || message.recipient == as_id)
        || envelope
            .asks
            .iter()
            .any(|ask| ask.asker == as_id || ask.askee == as_id)
        || envelope.ask_groups.iter().any(|group| group.asker == as_id);
    if conflicts {
        bail!(
            "session import --as '{as_id}' collides with a distinct source actor; choose an unused target identity"
        );
    }
    Ok(())
}

/// Bound every message field before any store write (the central import invariant).
fn validate_messages(
    msgs: &[ExportedMessage],
    source_identity: &str,
    asks: &[ExportedAsk],
) -> Result<()> {
    let mut by_id = HashMap::with_capacity(msgs.len());
    let mut keys = HashSet::with_capacity(msgs.len());
    let ask_question_ids: HashSet<i64> = asks.iter().map(|ask| ask.question_msg_id).collect();
    for m in msgs {
        if m.id <= 0 || by_id.insert(m.id, m).is_some() {
            bail!("imported messages must have unique positive source ids");
        }
        check_ident("message sender", &m.sender)?;
        check_ident("message recipient", &m.recipient)?;
        check_body(&m.body)?;
        check_subject(m.subject.as_deref()).context("invalid imported message subject")?;
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
        let effective_key = m
            .idempotency_key
            .clone()
            .unwrap_or_else(|| synth_idempotency_key(source_identity, m.id));
        if !keys.insert(effective_key) {
            bail!("imported messages carry duplicate effective idempotency keys");
        }
        if let Some(priority) = &m.priority {
            validate_import_priority(priority)?;
        }
        if m.reply_ttl != 0 && !weave_core::model::ttl_valid(m.reply_ttl) {
            bail!("imported reply carries an invalid ttl");
        }
        if m.in_reply_to.is_none() && m.reply_ttl != 0 {
            bail!("imported top-level message cannot carry reply_ttl");
        }
        if m.in_reply_to.is_some() && m.configured_send.is_some() {
            bail!("imported reply cannot also carry configured_send");
        }
        if let Some(configured) = &m.configured_send {
            validate_import_priority(&configured.priority)?;
            if configured.ttl != 0 && !weave_core::model::ttl_valid(configured.ttl) {
                bail!("imported configured send carries an invalid ttl");
            }
            if configured.dedup_idle && configured.supersedes.is_some() {
                bail!("imported configured send cannot combine dedup_idle and supersedes");
            }
        }
    }
    for m in msgs {
        let Some(parent_id) = m.in_reply_to else {
            continue;
        };
        let parent = by_id.get(&parent_id).ok_or_else(|| {
            anyhow::anyhow!(
                "imported reply {} references missing parent {parent_id}",
                m.id
            )
        })?;
        if parent.id >= m.id {
            bail!("imported reply parent must have an earlier source id");
        }
        let expected_recipient = if parent.sender == m.sender {
            &parent.recipient
        } else {
            &parent.sender
        };
        if &m.recipient != expected_recipient {
            bail!("imported reply recipient does not match its parent route");
        }
        if !ask_question_ids.contains(&m.id)
            && m.subject != weave_core::store::reply_subject(parent.subject.as_deref())
        {
            bail!("imported reply subject does not match its parent");
        }
    }
    for m in msgs {
        let Some(predecessor_id) = m
            .configured_send
            .as_ref()
            .and_then(|configured| configured.supersedes)
        else {
            continue;
        };
        let predecessor = by_id.get(&predecessor_id).ok_or_else(|| {
            anyhow::anyhow!(
                "imported configured send {} references missing predecessor {predecessor_id}",
                m.id
            )
        })?;
        if predecessor.id >= m.id {
            bail!("imported configured send predecessor must have an earlier source id");
        }
        if predecessor.sender != m.sender {
            bail!("imported configured send predecessor must have the same sender");
        }
        if predecessor.recipient != m.recipient {
            bail!("imported configured send predecessor must have the same recipient");
        }
    }
    for m in msgs {
        let Some(successor_id) = m.superseded_by else {
            continue;
        };
        let successor = by_id.get(&successor_id).ok_or_else(|| {
            anyhow::anyhow!(
                "imported message {} references missing successor {successor_id}",
                m.id
            )
        })?;
        if successor.id <= m.id {
            bail!("imported message successor must have a later source id");
        }
        if successor.sender != m.sender {
            bail!("imported message successor must have the same sender");
        }
        if successor.recipient != m.recipient {
            bail!("imported message successor must have the same recipient");
        }
    }
    Ok(())
}

fn validate_import_priority(priority: &str) -> Result<()> {
    if matches!(priority, "low" | "normal" | "high" | "urgent") {
        Ok(())
    } else {
        bail!("imported message carries an invalid priority")
    }
}

/// WL-040b: bound every imported ask field BEFORE any store write (the central
/// import invariant; the store seam re-validates too, defense-in-depth). The ask id
/// is regenerated on import, so a hostile source `id` never reaches a query — but we
/// still bound asker/askee (identity shape), subject/options/close_note (length),
/// state (must parse to the enum — unknown REJECTED), and parent_id (`askm_` shape).
fn validate_asks(asks: &[ExportedAsk]) -> Result<()> {
    let mut ask_ids = HashSet::with_capacity(asks.len());
    for a in asks {
        check_ident("ask asker", &a.asker)?;
        check_ident("ask askee", &a.askee)?;
        // The source id is informational (regenerated on import). Still reject an
        // egregiously malformed one early so a hostile file fails loudly.
        if !ask_id_valid(&a.id) {
            bail!("imported ask carries an invalid id");
        }
        if !ask_ids.insert(a.id.as_str()) {
            bail!("imported asks carry a duplicate source id");
        }
        let state = AskState::from_str(&a.state)
            .map_err(|_| anyhow::anyhow!("imported ask carries an unknown state '{}'", a.state))?;
        if !matches!(a.kind.as_str(), "free_text" | "choice" | "tool_permission") {
            bail!(
                "imported ask carries an unknown kind '{}'; expected a canonical ask kind",
                a.kind
            );
        }
        check_subject(a.subject.as_deref()).context("invalid imported ask subject")?;
        if let Some(o) = &a.options {
            check_body(o).context("invalid imported ask options")?;
        }
        if let Some(c) = &a.close_note {
            if c.len() > MAX_BODY {
                bail!(
                    "imported ask close_note is too long ({} bytes; max {MAX_BODY})",
                    c.len()
                );
            }
        }
        if let Some(rt) = &a.reply_to {
            if !ask_id_valid(rt) {
                bail!("imported ask carries an invalid reply_to id");
            }
        }
        if let Some(p) = &a.parent_id {
            if !ask_many_id_valid(p) {
                bail!("imported ask carries an invalid parent_id");
            }
        }
        weave_core::store::validate_imported_ask_lifecycle(
            a.question_msg_id,
            a.answer_msg_id,
            state,
            a.close_note.as_deref(),
            a.opened_ts,
            a.updated_ts,
            a.closed_ts,
        )?;
    }
    for ask in asks {
        if let Some(reply_to) = ask.reply_to.as_deref() {
            if reply_to == ask.id {
                bail!("imported ask cannot chain to itself");
            }
            if !ask_ids.contains(reply_to) {
                bail!("imported ask reply_to references a missing source ask");
            }
        }
    }
    // A multi-node cycle cannot be produced by the normal append-only ask API and
    // has no parent-first materialization order. Reject it during the all-fields
    // preflight so no messages are written before the problem is discovered.
    let mut ordered = HashSet::with_capacity(asks.len());
    loop {
        let before = ordered.len();
        for ask in asks {
            if ask
                .reply_to
                .as_deref()
                .is_none_or(|parent| ordered.contains(parent))
            {
                ordered.insert(ask.id.as_str());
            }
        }
        if ordered.len() == asks.len() {
            break;
        }
        if ordered.len() == before {
            bail!("imported ask reply_to chain contains a cycle");
        }
    }
    Ok(())
}

/// Validate every cross-record ask invariant before message import begins. Missing
/// message rows remain a supported retention case and make that ask (plus chained
/// descendants) unreconstructable, but every row that is present must be a faithful
/// question/answer/group/thread representation.
fn validate_ask_relations(
    messages: &[ExportedMessage],
    asks: &[ExportedAsk],
    groups: &[ExportedAskGroup],
) -> Result<()> {
    let by_message: HashMap<i64, &ExportedMessage> = messages
        .iter()
        .map(|message| (message.id, message))
        .collect();
    let by_ask: HashMap<&str, &ExportedAsk> =
        asks.iter().map(|ask| (ask.id.as_str(), ask)).collect();
    let by_group: HashMap<&str, &ExportedAskGroup> = groups
        .iter()
        .map(|group| (group.parent_id.as_str(), group))
        .collect();
    let present_messages: HashSet<i64> = by_message.keys().copied().collect();
    let reconstructable: HashSet<&str> = reconstructable_asks(asks, &present_messages)
        .into_iter()
        .map(|ask| ask.id.as_str())
        .collect();

    let mut claimed_messages: HashMap<i64, (&str, &str)> = HashMap::new();
    let mut group_askees: HashMap<&str, HashSet<&str>> = HashMap::new();
    for ask in asks {
        for (role, message_id) in std::iter::once(("question", ask.question_msg_id))
            .chain(ask.answer_msg_id.map(|id| ("answer", id)))
        {
            if let Some((prior_ask, prior_role)) =
                claimed_messages.insert(message_id, (ask.id.as_str(), role))
            {
                bail!(
                    "imported ask message {message_id} is claimed as {prior_role} by '{prior_ask}' and as {role} by '{}'",
                    ask.id
                );
            }
        }

        if !reconstructable.contains(ask.id.as_str()) {
            continue;
        }

        let question = by_message.get(&ask.question_msg_id).copied();
        if let Some(question) = question {
            if question.sender != ask.asker || question.recipient != ask.askee {
                bail!("imported ask question route does not match asker/askee");
            }
            if question.subject != ask.subject {
                bail!("imported ask subject does not match its question message");
            }
            if question.ts != ask.opened_ts {
                bail!("imported ask question timestamp does not match opened_ts");
            }
        }

        if let Some(reply_to) = ask.reply_to.as_deref() {
            let parent = by_ask[reply_to];
            let same_pair = (parent.asker == ask.asker && parent.askee == ask.askee)
                || (parent.asker == ask.askee && parent.askee == ask.asker);
            if !same_pair {
                bail!("imported chained ask crosses unrelated parties");
            }
            if parent.state != AskState::Acked.as_str() {
                bail!("imported chained ask requires an acked parent");
            }
            if parent.updated_ts > ask.opened_ts
                || parent.closed_ts.is_none_or(|closed| closed > ask.opened_ts)
            {
                bail!("imported chained ask timestamps precede its parent closure");
            }
            if let Some(question) = question {
                let expected_parent = parent.answer_msg_id.unwrap_or(parent.question_msg_id);
                if question.in_reply_to != Some(expected_parent) {
                    bail!("imported chained ask question does not link to its parent thread");
                }
            }
        } else if question.is_some_and(|question| question.in_reply_to.is_some()) {
            bail!("imported root ask question cannot carry in_reply_to");
        }

        if let Some(answer_id) = ask.answer_msg_id {
            if let Some(answer) = by_message.get(&answer_id).copied() {
                if answer.sender != ask.askee || answer.recipient != ask.asker {
                    bail!("imported ask answer route does not match askee/asker");
                }
                if answer.in_reply_to != Some(ask.question_msg_id) {
                    bail!("imported ask answer does not reply to its question");
                }
                let expected_subject = weave_core::store::reply_subject(ask.subject.as_deref());
                if answer.subject != expected_subject {
                    bail!("imported ask answer subject does not match its question");
                }
                if answer.ts < ask.opened_ts || answer.ts > ask.updated_ts {
                    bail!("imported ask answer timestamp is outside its lifecycle");
                }
                if ask.state == AskState::Answered.as_str() && answer.ts != ask.updated_ts {
                    bail!("imported answered ask timestamp does not match its answer");
                }
            }
        }

        if let Some(parent_id) = ask.parent_id.as_deref() {
            let group = by_group
                .get(parent_id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("imported ask references a missing ask group"))?;
            if ask.asker != group.asker
                || ask.subject != group.subject
                || ask.opened_ts != group.opened_ts
                || ask.kind != AskKind::FreeText.as_str()
                || ask.options.is_some()
                || ask.reply_to.is_some()
            {
                bail!("imported ask is incoherent with its ask group");
            }
            if question.is_some_and(|question| {
                question.body != group.body || question.in_reply_to.is_some()
            }) {
                bail!("imported ask question is incoherent with its ask group");
            }
            let askees = group_askees.entry(parent_id).or_default();
            if !askees.insert(ask.askee.as_str()) {
                bail!("imported ask group contains a duplicate askee");
            }
            if askees.len() > group.target_count as usize {
                bail!("imported ask group has more children than target_count");
            }
        }
    }
    Ok(())
}

/// WL-040b: bound every imported ask-many group field BEFORE any store write. The
/// parent id is regenerated on import; still reject a malformed source id early.
fn validate_ask_groups(groups: &[ExportedAskGroup]) -> Result<()> {
    let mut parent_ids = HashSet::with_capacity(groups.len());
    for g in groups {
        check_ident("ask group asker", &g.asker)?;
        if !ask_many_id_valid(&g.parent_id) {
            bail!("imported ask group carries an invalid parent_id");
        }
        if !parent_ids.insert(g.parent_id.as_str()) {
            bail!("imported ask groups carry a duplicate parent_id");
        }
        check_body(&g.body)?;
        check_subject(g.subject.as_deref()).context("invalid imported ask-group subject")?;
        if !(1..=weave_core::store::MAX_ASK_MANY_TARGETS as i64).contains(&g.target_count) {
            bail!(
                "imported ask group target_count must be between 1 and {}",
                weave_core::store::MAX_ASK_MANY_TARGETS
            );
        }
    }
    Ok(())
}

/// Validate every memory field before any database or filesystem write. Session
/// interchange must reject values the interactive memory API would silently
/// normalize, otherwise the imported entry would not be a faithful round trip.
fn validate_memory(mem: &[ExportedMemory]) -> Result<()> {
    let mut logical_keys = HashSet::with_capacity(mem.len());
    let mut scopes = HashSet::with_capacity(mem.len());
    for e in mem {
        let scope = scope_from_strings(&e.scope_kind, &e.scope_name)?;
        memory::validate_portable_entry(&scope, &e.key, &e.title, &e.tags, &e.body)
            .with_context(|| format!("invalid imported memory entry '{}'", e.key))?;
        if !logical_keys.insert((e.scope_kind.as_str(), e.scope_name.as_str(), e.key.as_str())) {
            bail!(
                "session import contains duplicate memory key ({}, {}, {})",
                e.scope_kind,
                e.scope_name,
                e.key
            );
        }
        scopes.insert((e.scope_kind.as_str(), e.scope_name.as_str()));
    }
    if scopes.len() > MAX_SESSION_MEMORY_ENTRIES {
        bail!(
            "session memory scopes exceed {} entries",
            MAX_SESSION_MEMORY_ENTRIES
        );
    }
    Ok(())
}

// ===========================================================================
// Path guards (mirror backup.rs)
// ===========================================================================

/// Hard upper bound on a session document. An import file is untrusted; an
/// unbounded one is a RAM DoS once read into memory + parsed. This byte ceiling is
/// paired with parser-time collection bounds and the existing per-field caps.
const MAX_IMPORT_FILE_BYTES: usize = 256 * 1024 * 1024;

fn validate_export_size(bytes: usize) -> Result<()> {
    if bytes > MAX_IMPORT_FILE_BYTES {
        bail!(
            "session export is too large ({bytes} bytes; max {MAX_IMPORT_FILE_BYTES}); reduce --limit"
        );
    }
    Ok(())
}

/// Read one untrusted import without ever allocating beyond the documented cap.
/// The metadata check cheaply rejects ordinary oversized files; `take(MAX + 1)`
/// remains the authoritative bound if the file grows after that check or reports
/// a synthetic length (for example, a procfs-style file).
fn read_import_bounded(in_path: &Path) -> Result<String> {
    let file = File::open(in_path)
        .with_context(|| format!("opening session import file {}", in_path.display()))?;
    let metadata = file.metadata().with_context(|| {
        format!(
            "reading metadata for session import file {}",
            in_path.display()
        )
    })?;
    if metadata.len() > MAX_IMPORT_FILE_BYTES as u64 {
        bail!(
            "session import file is too large ({} bytes; max {MAX_IMPORT_FILE_BYTES})",
            metadata.len()
        );
    }

    let mut raw = Vec::new();
    file.take(MAX_IMPORT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .with_context(|| format!("reading session import file {}", in_path.display()))?;
    if raw.len() > MAX_IMPORT_FILE_BYTES {
        bail!(
            "session import file is too large (more than {MAX_IMPORT_FILE_BYTES} bytes; max {MAX_IMPORT_FILE_BYTES})"
        );
    }
    String::from_utf8(raw).with_context(|| {
        format!(
            "session import file is not valid UTF-8: {}",
            in_path.display()
        )
    })
}

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

/// Owns an exclusively-created sibling temp and removes it on every error path.
struct TempExport {
    path: std::path::PathBuf,
    file: Option<File>,
}

impl Drop for TempExport {
    fn drop(&mut self) {
        let _ = self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Create a collision-resistant sibling temp with `create_new` so an existing
/// symlink or file can never be followed or truncated. On Unix the explicit mode
/// prevents session contents from being exposed even under a permissive umask.
fn create_export_temp(base: &Path) -> Result<TempExport> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".weave-session.{}.{}.{}.tmp",
            std::process::id(),
            nonce,
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => {
                let temp = TempExport {
                    path,
                    file: Some(file),
                };
                #[cfg(unix)]
                temp.file
                    .as_ref()
                    .expect("new temp owns its file")
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .context("setting private session export temp permissions")?;
                return Ok(temp);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "creating private session export temp in {}",
                        parent.display()
                    )
                });
            }
        }
    }
    bail!(
        "could not create a unique session export temp in {}",
        parent.display()
    )
}

/// Publish a fully-written sibling temp. `hard_link` is the portable atomic
/// no-clobber primitive: unlike `rename`, it fails if another process creates the
/// target after the initial path validation. Forced exports use atomic rename.
fn publish_export(temp: &mut TempExport, out: &Path, force: bool) -> Result<()> {
    temp.file.take();
    if force {
        std::fs::rename(&temp.path, out)
            .with_context(|| format!("renaming {} -> {}", temp.path.display(), out.display()))?;
    } else {
        match std::fs::hard_link(&temp.path, out) {
            Ok(()) => {
                std::fs::remove_file(&temp.path).with_context(|| {
                    format!(
                        "removing published session export temp {}",
                        temp.path.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "refusing to overwrite existing file {} (pass --force to overwrite)",
                    out.display()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("publishing session export to {}", out.display()));
            }
        }
    }

    // Make the directory entry durable after the file data itself was synced.
    #[cfg(unix)]
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing session export directory {}", parent.display()))?;
    }
    Ok(())
}

fn write_export_atomically(out: &Path, bytes: &[u8], force: bool) -> Result<()> {
    let mut temp = create_export_temp(out)?;
    let file = temp.file.as_mut().expect("new temp owns its file");
    file.write_all(bytes)
        .with_context(|| format!("writing session export to {}", temp.path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing session export temp {}", temp.path.display()))?;
    publish_export(&mut temp, out, force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_force_publish_cannot_clobber_target_created_after_temp() {
        let dir = std::env::temp_dir().join(format!(
            "weave-session-publish-race-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("session.json");
        let mut temp = create_export_temp(&out).unwrap();
        let temp_path = temp.path.clone();
        temp.file
            .as_mut()
            .unwrap()
            .write_all(b"new session")
            .unwrap();
        temp.file.as_ref().unwrap().sync_all().unwrap();

        // Models another process winning the path race after validate_out_path.
        std::fs::write(&out, b"concurrent owner").unwrap();
        let error = publish_export(&mut temp, &out, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert_eq!(std::fs::read(&out).unwrap(), b"concurrent owner");
        drop(temp);
        assert!(
            !temp_path.exists(),
            "failed publication must clean its temp"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_size_must_remain_importable() {
        validate_export_size(MAX_IMPORT_FILE_BYTES).unwrap();
        let error = validate_export_size(MAX_IMPORT_FILE_BYTES + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("reduce --limit"), "{error}");
    }

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
}
