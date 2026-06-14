//! WL-040: pure (de)serialize layer for the canonical, schema-versioned session
//! interchange format.
//!
//! Everything here is **I/O-free and pure**: it builds an in-memory
//! [`SessionExport`] envelope from data already fetched through the `Store`
//! trait + the filesystem memory store (the bin-layer `weave session export`
//! handler does the reads), and turns it into one canonical JSON document — and
//! back. That purity is what makes the format unit-testable with `Vec<Message>`
//! / `Vec<Ask>` / `Vec<ExportedMemory>` and no DB, and it keeps the layer DAG
//! intact (this module reads no DB, touches no filesystem, opens no socket).
//!
//! This is distinct from the other two "export" surfaces:
//! - [`crate::export`] (WL-034) renders a **presentation** HTML bundle.
//! - [`crate::archive`] (WL-035) packages a **byte-exact host-local** DB snapshot.
//! - WL-040 (this module) is the **logical, portable, versioned interchange**
//!   format whose row ids and minted correlation ids deliberately do NOT carry
//!   across instances — import re-mints fresh local ids via `Store::send`.
//!
//! ## Untrusted input
//!
//! An imported document is **untrusted external input**. This module's
//! [`from_json`] validates the format magic + schema version and is tolerant of
//! unknown fields (`#[serde(default)]`, the established additive pattern), but it
//! does NOT enforce business caps — the bin-layer import handler bounds every
//! field (`check_ident`, `MAX_BODY`, id-shape) BEFORE any value reaches the store.
//! Treat the structs here as a parser, not a trust boundary.

use serde::{Deserialize, Serialize};

/// Format discriminator / magic. A document whose `weave_session_export` value is
/// not exactly this is rejected by [`from_json`] — it is not a weave session
/// export. Bumping the *format* (an incompatible envelope shape) would change this
/// value; additive field growth bumps [`SCHEMA_VERSION`] instead.
pub const FORMAT_TAG: u32 = 1;

/// Schema version of the envelope payload. Additive growth (a new optional block
/// or field) bumps this; [`from_json`] accepts any document with
/// `schema_version <= SCHEMA_VERSION` and ignores unknown fields, so an older
/// weave never chokes on a newer-but-compatible export, and a newer weave can read
/// an older one. A document with a HIGHER schema version than this build knows is
/// rejected (forward-compat guard — we will not silently drop data we cannot model).
pub const SCHEMA_VERSION: u32 = 1;

/// The canonical interchange envelope. Field order here is the stable key order of
/// the emitted JSON (serde preserves struct field declaration order), so two
/// exports of the same logical state are byte-identical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionExport {
    /// Format magic — must equal [`FORMAT_TAG`] on import.
    pub weave_session_export: u32,
    /// Envelope schema version — must be `<= SCHEMA_VERSION` on import.
    pub schema_version: u32,
    /// The exported identity (advisory provenance only; import remaps via `--as`).
    pub identity: String,
    /// UNIX-seconds wall clock at export time (advisory).
    pub exported_at: i64,
    /// The portable message set (the core payload; imported via `Store::send`).
    #[serde(default)]
    pub messages: Vec<ExportedMessage>,
    /// The tracked-ask threads, replayed faithfully on import via
    /// [`crate::store::Store::import_ask`] (WL-040b): each ask is re-materialized in
    /// its exported `AskState` (open / answered / acked) with its message links
    /// remapped to the freshly re-minted local ids. `#[serde(default)]` keeps an
    /// older document that omits the block deserializable.
    #[serde(default)]
    pub asks: Vec<ExportedAsk>,
    /// The ask-many PARENT anchor rows (P2 broadcast-ask groups), replayed before
    /// the child asks that reference them so `parent_id` linkage survives the import
    /// (WL-040b). `#[serde(default)]` keeps an older document that omits the block
    /// deserializable.
    #[serde(default)]
    pub ask_groups: Vec<ExportedAskGroup>,
    /// The mesh memory entries (filesystem-backed scoped memory; full round-trip).
    /// `#[serde(default)]` keeps an older document that omits the block
    /// deserializable.
    #[serde(default)]
    pub memory: Vec<ExportedMemory>,
}

/// One message in the portable set. Mirrors the durable fields of
/// [`crate::model::Message`]; `id` is the SOURCE row id (used only to synthesize a
/// deterministic dedup key for keyless legacy messages — the importer mints a
/// fresh local id). Every added field is `#[serde(default)]` so an older document
/// stays deserializable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportedMessage {
    /// Source-store row id (provenance + synth-key seed; NOT carried to the target).
    pub id: i64,
    pub ts: i64,
    pub sender: String,
    pub recipient: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub body: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
}

/// One tracked ask, replayed faithfully on import (WL-040b) via the dual-backend
/// `Store::import_ask`, which materializes the row directly in its exported
/// [`crate::model::AskState`] (out-of-order, bypassing the create→answer→ack
/// lifecycle) with `question_msg_id`/`answer_msg_id` remapped to the freshly
/// re-minted local message ids. Every field that was added after the first cut is
/// `#[serde(default)]`, so an older export (which omitted `kind`/`options`/
/// `reply_to`/`close_note`/`parent_id`) stays deserializable and replays with the
/// safe defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportedAsk {
    pub id: String,
    pub question_msg_id: i64,
    #[serde(default)]
    pub answer_msg_id: Option<i64>,
    pub asker: String,
    pub askee: String,
    #[serde(default)]
    pub subject: Option<String>,
    /// Lifecycle state label (`open`/`answered`/`acked`).
    pub state: String,
    /// Structured kind label (`free_text`/`choice`/`tool_permission`); defaults to
    /// `free_text` for an older export that omitted it.
    #[serde(default = "default_ask_kind")]
    pub kind: String,
    /// Kind-specific payload (newline-separated choices, or `tool_name\ntool_args`).
    #[serde(default)]
    pub options: Option<String>,
    /// Prior ask id this one chains/closes (`None` for a root ask). Carried for
    /// fidelity; remapped to the new local ask id on import when resolvable.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Optional closing note from `ack`.
    #[serde(default)]
    pub close_note: Option<String>,
    pub opened_ts: i64,
    pub updated_ts: i64,
    #[serde(default)]
    pub closed_ts: Option<i64>,
    /// Parent ask-many group id (`askm_<...>`) this ask is a child of, or `None` for
    /// a standalone ask. Remapped to the replayed group id on import.
    #[serde(default)]
    pub parent_id: Option<String>,
}

/// Default `ExportedAsk::kind` for an older export that predates the WL-040b field:
/// the legacy `free_text` kind (matches `AskKind::default()`).
fn default_ask_kind() -> String {
    "free_text".to_string()
}

/// One ask-many PARENT anchor row (the `ask_groups` table), recorded so a
/// broadcast-ask group's `parent_id` linkage and totality (`target_count`) survive
/// the import (WL-040b). Replayed before the child asks that reference it via the
/// dual-backend `Store::import_ask_group`. Every added field is `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportedAskGroup {
    pub parent_id: String,
    pub asker: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub body: String,
    pub opened_ts: i64,
    pub target_count: i64,
}

/// One mesh-memory entry (filesystem-backed scoped memory; full round-trip).
/// `scope_kind` is one of `global`/`project`/`persona`/`orchestrator`; `scope_name`
/// is the sub-scope (empty for `global`). The importer reconstructs the
/// [`crate::memory::MemoryScope`] from these two fields and writes via
/// `memory::memory_write`, which re-bounds every field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportedMemory {
    pub scope_kind: String,
    #[serde(default)]
    pub scope_name: String,
    pub key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub body: String,
}

/// Build a canonical envelope from already-fetched, pure data. No I/O.
#[allow(clippy::too_many_arguments)]
pub fn serialize_session(
    identity: &str,
    exported_at: i64,
    messages: Vec<ExportedMessage>,
    asks: Vec<ExportedAsk>,
    ask_groups: Vec<ExportedAskGroup>,
    memory: Vec<ExportedMemory>,
) -> SessionExport {
    SessionExport {
        weave_session_export: FORMAT_TAG,
        schema_version: SCHEMA_VERSION,
        identity: identity.to_string(),
        exported_at,
        messages,
        asks,
        ask_groups,
        memory,
    }
}

/// Serialize an envelope to canonical, pretty JSON. Stable key order (struct field
/// order); ends with a trailing newline (POSIX text file convention).
pub fn to_json(export: &SessionExport) -> anyhow::Result<String> {
    let mut s = serde_json::to_string_pretty(export)?;
    s.push('\n');
    Ok(s)
}

/// Parse + validate an interchange document. Rejects a wrong/missing format magic
/// and a `schema_version` greater than this build's [`SCHEMA_VERSION`]
/// (forward-compat guard). Unknown extra fields are ignored (additive tolerance).
/// This validates ONLY the format frame — per-field business caps are enforced by
/// the bin-layer import handler before any value reaches the store.
pub fn from_json(s: &str) -> anyhow::Result<SessionExport> {
    let export: SessionExport = serde_json::from_str(s)
        .map_err(|e| anyhow::anyhow!("not a valid weave session export (parse error: {e})"))?;
    if export.weave_session_export != FORMAT_TAG {
        anyhow::bail!(
            "unrecognized session export magic (got {}, expected {FORMAT_TAG}); not a weave \
             session export",
            export.weave_session_export
        );
    }
    if export.schema_version > SCHEMA_VERSION {
        anyhow::bail!(
            "session export schema_version {} is newer than this weave supports (max \
             {SCHEMA_VERSION}); upgrade weave to import it",
            export.schema_version
        );
    }
    Ok(export)
}

/// Deterministic synthetic idempotency key for a keyless legacy message:
/// `wl040:<source_identity>:<source_id>`. Re-importing the same document twice
/// collides on this key (the existing global-unique dedup makes the second import a
/// no-op), so import stays idempotent even for messages that carried no key in the
/// source store. Bounded `[A-Za-z0-9_:]`, well under `MAX_IDEMPOTENCY_KEY_LEN`; the
/// identity is sanitized to that alphabet so a hostile source identity cannot smuggle
/// a metachar into the synthesized key.
pub fn synth_idempotency_key(source_identity: &str, source_id: i64) -> String {
    let ident: String = source_identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("wl040:{ident}:{source_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_msg(id: i64, key: Option<&str>) -> ExportedMessage {
        ExportedMessage {
            id,
            ts: 1000 + id,
            sender: "alice".into(),
            recipient: "bob".into(),
            subject: Some("hi".into()),
            body: format!("body {id}"),
            idempotency_key: key.map(|s| s.to_string()),
            trace_id: Some("trace_1".into()),
            priority: Some("normal".into()),
        }
    }

    #[test]
    fn round_trip_preserves_messages() {
        let msgs = vec![sample_msg(1, Some("k1")), sample_msg(2, None)];
        let env = serialize_session("alice", 12345, msgs.clone(), vec![], vec![], vec![]);
        let json = to_json(&env).unwrap();
        let back = from_json(&json).unwrap();
        assert_eq!(back.identity, "alice");
        assert_eq!(back.exported_at, 12345);
        assert_eq!(back.messages, msgs);
        assert_eq!(back, env);
    }

    #[test]
    fn round_trip_preserves_asks_and_memory() {
        let asks = vec![ExportedAsk {
            id: "ask_1_2".into(),
            question_msg_id: 1,
            answer_msg_id: Some(2),
            asker: "alice".into(),
            askee: "bob".into(),
            subject: None,
            state: "answered".into(),
            kind: "choice".into(),
            options: Some("yes\nno".into()),
            reply_to: None,
            close_note: None,
            opened_ts: 100,
            updated_ts: 200,
            closed_ts: None,
            parent_id: Some("askm_1_2".into()),
        }];
        let groups = vec![ExportedAskGroup {
            parent_id: "askm_1_2".into(),
            asker: "alice".into(),
            subject: Some("poll".into()),
            body: "yes or no?".into(),
            opened_ts: 100,
            target_count: 2,
        }];
        let mem = vec![ExportedMemory {
            scope_kind: "global".into(),
            scope_name: String::new(),
            key: "patterns".into(),
            title: "Patterns".into(),
            tags: vec!["rust".into()],
            body: "Always use types.".into(),
        }];
        let env = serialize_session(
            "alice",
            1,
            vec![],
            asks.clone(),
            groups.clone(),
            mem.clone(),
        );
        let back = from_json(&to_json(&env).unwrap()).unwrap();
        assert_eq!(back.asks, asks);
        assert_eq!(back.ask_groups, groups);
        assert_eq!(back.memory, mem);
    }

    #[test]
    fn older_export_without_new_ask_fields_defaults() {
        // An export predating the WL-040b ExportedAsk fields must still parse, with
        // kind defaulting to `free_text` and the new optionals to None.
        let json = format!(
            r#"{{"weave_session_export":{FORMAT_TAG},"schema_version":{SCHEMA_VERSION},"identity":"a","exported_at":0,"messages":[],"asks":[{{"id":"ask_1_2","question_msg_id":1,"asker":"a","askee":"b","state":"open","opened_ts":1,"updated_ts":1}}],"memory":[]}}"#
        );
        let env = from_json(&json).unwrap();
        assert_eq!(env.asks.len(), 1);
        assert_eq!(env.asks[0].kind, "free_text");
        assert_eq!(env.asks[0].options, None);
        assert_eq!(env.asks[0].parent_id, None);
        assert!(env.ask_groups.is_empty());
    }

    #[test]
    fn empty_session_round_trips() {
        let env = serialize_session("solo", 7, vec![], vec![], vec![], vec![]);
        let back = from_json(&to_json(&env).unwrap()).unwrap();
        assert_eq!(back, env);
        assert!(back.messages.is_empty());
        assert!(back.asks.is_empty());
        assert!(back.ask_groups.is_empty());
        assert!(back.memory.is_empty());
    }

    #[test]
    fn from_json_rejects_wrong_magic() {
        let json = r#"{"weave_session_export":999,"schema_version":1,"identity":"a","exported_at":0,"messages":[],"asks":[],"memory":[]}"#;
        let err = from_json(json).unwrap_err().to_string();
        assert!(err.contains("magic"), "got: {err}");
    }

    #[test]
    fn from_json_rejects_future_schema_version() {
        let json = format!(
            r#"{{"weave_session_export":{FORMAT_TAG},"schema_version":{},"identity":"a","exported_at":0,"messages":[],"asks":[],"memory":[]}}"#,
            SCHEMA_VERSION + 1
        );
        let err = from_json(&json).unwrap_err().to_string();
        assert!(err.contains("newer than this weave"), "got: {err}");
    }

    #[test]
    fn from_json_tolerates_unknown_fields() {
        // An additive future field must not break an older importer.
        let json = format!(
            r#"{{"weave_session_export":{FORMAT_TAG},"schema_version":{SCHEMA_VERSION},"identity":"a","exported_at":0,"messages":[],"asks":[],"memory":[],"future_block":[1,2,3]}}"#
        );
        let env = from_json(&json).unwrap();
        assert_eq!(env.identity, "a");
    }

    #[test]
    fn from_json_rejects_garbage() {
        assert!(from_json("not json at all").is_err());
        assert!(from_json("{}").is_err()); // missing required fields
    }

    #[test]
    fn synth_key_is_deterministic_and_bounded() {
        let a = synth_idempotency_key("alice", 42);
        let b = synth_idempotency_key("alice", 42);
        assert_eq!(a, b);
        assert_eq!(a, "wl040:alice:42");
        assert!(a.len() <= crate::model::MAX_IDEMPOTENCY_KEY_LEN);
        assert!(crate::model::idempotency_key_valid(&a));
    }

    #[test]
    fn synth_key_sanitizes_hostile_identity() {
        let k = synth_idempotency_key("a; DROP TABLE messages;--", 1);
        // Metachars are replaced with '_', so the key stays in the safe alphabet.
        assert!(k
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':'));
        assert!(crate::model::idempotency_key_valid(&k));
    }

    #[test]
    fn synth_key_differs_per_source_id() {
        assert_ne!(
            synth_idempotency_key("alice", 1),
            synth_idempotency_key("alice", 2)
        );
    }
}
