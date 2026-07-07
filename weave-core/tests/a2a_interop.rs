//! Additive RED suite — A2A v1.0 interop convergence criteria for weave-core.
//!
//! Context (plan cycle 4, test-coverage dimension): weave carries agent-to-agent
//! traffic using its OWN `Intent` wire schema (`weave-core/src/model.rs:216`) over
//! a SQLite-mailbox + HTTP-push substrate. It does NOT speak the Linux-Foundation
//! **A2A (Agent2Agent) v1.0** interop standard (JSON-RPC 2.0 envelope + A2A
//! `Message` object with `role`/`parts`/`messageId`/`kind`), per
//! `research/weave.trends.md` §A1. The architect's direction is a **strict-upgrade
//! A2A adapter** (`to_a2a` / `from_a2a`) that maps `Intent` <-> an A2A message and
//! round-trips the core fields (id / from / to / subject / body) — ADD interop
//! without removing the mailbox transport.
//!
//! These tests encode the falsifiable CONTRACT that adapter MUST satisfy. They are
//! authored to **compile and RUN against the EXISTING `Intent`** (serde_json over
//! the current derive) and **FAIL on assertion** today (RED for the right reason —
//! the A2A mapping is unbuilt — NOT a fail-to-compile). They turn GREEN only once
//! the A2A adapter (Feature Forge work item) re-shapes the on-the-wire form.
//!
//! Adapter-shape note: when the A2A adapter lands, `to_a2a`/`from_a2a` will exist
//! and these tests should be migrated to drive those fns directly; until then they
//! assert the wire SHAPE so they need no unbuilt symbol to compile.

use weave_core::model::Intent;

/// Build a representative cross-store `Intent` with all core fields populated.
/// Uses only existing public fields (`weave-core/src/model.rs:216`).
fn sample_intent() -> Intent {
    Intent {
        id: 7,
        ts: 1_700_000_000,
        to: "bob".to_string(),
        to_host: String::new(),
        from: "alice".to_string(),
        subject: Some("status".to_string()),
        body: "build is green".to_string(),
        sig: String::new(),
        idempotency_key: Some("idem-7".to_string()),
        trace_id: Some("trace-7".to_string()),
        priority: "normal".to_string(),
        ttl: 0,
    }
}

/// CRITERION A2A-1 (to_a2a — Message-object mapping):
/// Serializing an `Intent` for the wire MUST yield an **A2A v1.0 `Message` object**
/// (`kind == "message"`, a `role`, a `messageId`, and a `parts[]` array whose first
/// text part carries the body), with the Intent core fields mapped onto it
/// (id -> messageId, body -> parts[0].text).
///
/// RED today: the current `Intent` derive serializes a FLAT
/// `{"id","ts","to","from","subject","body",...}` shape with no `kind`/`role`/
/// `parts`/`messageId`, so every A2A-shape assertion below fails.
#[test]
fn intent_serializes_to_a2a_message_object() {
    let intent = sample_intent();
    let v: serde_json::Value = serde_json::to_value(&intent).expect("Intent serializes to JSON");

    // A2A v1.0 Message discriminator + role.
    assert_eq!(
        v.get("kind").and_then(|k| k.as_str()),
        Some("message"),
        "A2A-1: serialized Intent must carry A2A Message discriminator kind=\"message\"; \
         got flat Intent shape with keys {:?}",
        v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>())
    );
    assert!(
        v.get("role").and_then(|r| r.as_str()).is_some(),
        "A2A-1: A2A Message must carry a `role` (agent|user)"
    );

    // messageId maps from Intent.id.
    assert_eq!(
        v.get("messageId").and_then(|m| m.as_str()),
        Some("7"),
        "A2A-1: A2A `messageId` must map from Intent.id"
    );

    // parts[0].text maps from Intent.body.
    let first_text = v
        .get("parts")
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p0| p0.get("text"))
        .and_then(|t| t.as_str());
    assert_eq!(
        first_text,
        Some("build is green"),
        "A2A-1: A2A `parts[0].text` must map from Intent.body"
    );
}

/// CRITERION A2A-2 (from_a2a — inbound parse):
/// An inbound **A2A v1.0 `Message` JSON** MUST be parseable into a weave `Intent`
/// with the core fields recovered (messageId -> id source, role/parts -> body,
/// metadata.from/to -> from/to). The adapter is what bridges the field names; the
/// minimum falsifiable proof is that the A2A wire JSON deserializes into `Intent`
/// at all.
///
/// RED today: `Intent`'s required fields (`id:i64`, `ts`, `to`, `from`, `body`) do
/// not exist under those names in the A2A Message, so `from_value::<Intent>` errors
/// (missing field) — `is_ok()` is false.
#[test]
fn a2a_message_deserializes_into_intent() {
    // A representative A2A v1.0 Message (the body weave would RECEIVE from a
    // foreign A2A agent), with weave-relevant routing carried in `metadata`.
    let a2a_message = serde_json::json!({
        "kind": "message",
        "role": "agent",
        "messageId": "7",
        "parts": [ { "kind": "text", "text": "build is green" } ],
        "metadata": {
            "from": "alice",
            "to": "bob",
            "subject": "status"
        }
    });

    let parsed: Result<Intent, _> = serde_json::from_value(a2a_message);
    assert!(
        parsed.is_ok(),
        "A2A-2: an inbound A2A v1.0 Message must deserialize into a weave Intent \
         (needs a from_a2a adapter); got error: {:?}",
        parsed.err()
    );

    let intent = parsed.unwrap();
    assert_eq!(
        intent.from, "alice",
        "A2A-2: A2A metadata.from -> Intent.from"
    );
    assert_eq!(intent.to, "bob", "A2A-2: A2A metadata.to -> Intent.to");
    assert_eq!(
        intent.body, "build is green",
        "A2A-2: A2A parts[0].text -> Intent.body"
    );
    assert_eq!(
        intent.subject.as_deref(),
        Some("status"),
        "A2A-2: A2A metadata.subject -> Intent.subject"
    );
}

/// CRITERION A2A-3 (JSON-RPC 2.0 transport envelope):
/// To interoperate, weave's outbound A2A send MUST be framed as an **A2A v1.0
/// JSON-RPC 2.0 request** — `{"jsonrpc":"2.0","id":..,"method":"message/send",
/// "params":{"message":{..A2A Message..}}}` (research §A1: JSON-RPC/SSE/gRPC
/// bindings). This is the transport-framing layer, distinct from the Message
/// object mapping (A2A-1).
///
/// RED today: serializing an `Intent` produces a flat object with no `jsonrpc`,
/// `method`, or `params.message` — the envelope is absent.
#[test]
fn intent_frames_as_a2a_jsonrpc_request() {
    let intent = sample_intent();
    let v: serde_json::Value = serde_json::to_value(&intent).expect("Intent serializes to JSON");

    assert_eq!(
        v.get("jsonrpc").and_then(|j| j.as_str()),
        Some("2.0"),
        "A2A-3: outbound A2A frame must declare jsonrpc=\"2.0\""
    );
    assert_eq!(
        v.get("method").and_then(|m| m.as_str()),
        Some("message/send"),
        "A2A-3: outbound A2A frame must use method=\"message/send\""
    );
    assert!(
        v.pointer("/params/message")
            .map(|m| m.is_object())
            .unwrap_or(false),
        "A2A-3: A2A JSON-RPC request must wrap the Message under params.message"
    );
}
