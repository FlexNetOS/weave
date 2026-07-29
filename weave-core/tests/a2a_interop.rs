//! A2A v1.0 interop convergence criteria for weave-core.
//!
//! Context (plan cycle 4, test-coverage dimension): weave carries agent-to-agent
//! traffic using its OWN `Intent` wire schema (`weave-core/src/model.rs:256`) over
//! a SQLite-mailbox + HTTP-push substrate. It did NOT speak the Linux-Foundation
//! **A2A (Agent2Agent) v1.0** interop standard (JSON-RPC 2.0 envelope + A2A
//! `Message` object with `role`/`parts`/`messageId`/`kind`), per
//! `research/weave.trends.md` §A1. The architect's direction was a **strict-upgrade
//! A2A adapter** (`to_a2a` / `from_a2a`) that maps `Intent` <-> an A2A message and
//! round-trips the core fields (id / from / to / subject / body) — ADD interop
//! without removing the mailbox transport.
//!
//! These tests encode the falsifiable CONTRACT that adapter must satisfy.
//!
//! As the original RED suite's own adapter-shape note instructed — "when the A2A
//! adapter lands, `to_a2a`/`from_a2a` will exist and these tests should be migrated
//! to drive those fns directly" — they now drive the adapter rather than asserting
//! on `Intent`'s own serde output. That migration is required, not cosmetic: the
//! placeholder form asserted that `serde_json::to_value(&Intent)` must itself be
//! both a bare A2A `Message` *and* a JSON-RPC envelope, which would have re-shaped
//! the persisted mailbox row and the cross-store pull format — the very transport
//! the work item requires be left intact.

#![cfg(feature = "a2a")]

use weave_core::a2a::A2aMessage;
use weave_core::model::Intent;

/// Build a representative cross-store `Intent` with all core fields populated.
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
/// `Intent::to_a2a` MUST yield an **A2A v1.0 `Message` object** (`kind ==
/// "message"`, a `role`, a `messageId`, and a `parts[]` array whose first text part
/// carries the body), with the Intent core fields mapped onto it
/// (id -> messageId, body -> parts[0].text).
#[test]
fn intent_serializes_to_a2a_message_object() {
    let intent = sample_intent();
    let v: serde_json::Value =
        serde_json::to_value(intent.to_a2a()).expect("A2A message serializes to JSON");

    // A2A v1.0 Message discriminator + role.
    assert_eq!(
        v.get("kind").and_then(|k| k.as_str()),
        Some("message"),
        "A2A-1: A2A message must carry the discriminator kind=\"message\"; got keys {:?}",
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
/// An inbound **A2A v1.0 `Message` JSON** MUST parse into a weave `Intent` with the
/// core fields recovered (messageId -> id, parts -> body, metadata.from/to ->
/// from/to, metadata.subject -> subject).
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

    let parsed: Result<A2aMessage, _> = serde_json::from_value(a2a_message);
    assert!(
        parsed.is_ok(),
        "A2A-2: an inbound A2A v1.0 Message must deserialize; got error: {:?}",
        parsed.err()
    );

    let intent = Intent::from_a2a(&parsed.unwrap());
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
    assert_eq!(intent.id, 7, "A2A-2: A2A messageId -> Intent.id");
}

/// CRITERION A2A-3 (JSON-RPC 2.0 transport envelope):
/// weave's outbound A2A send MUST be framed as an **A2A v1.0 JSON-RPC 2.0
/// request** — `{"jsonrpc":"2.0","id":..,"method":"message/send",
/// "params":{"message":{..A2A Message..}}}` (research §A1: JSON-RPC/SSE/gRPC
/// bindings). This is the transport-framing layer, distinct from the Message
/// object mapping (A2A-1).
#[test]
fn intent_frames_as_a2a_jsonrpc_request() {
    let intent = sample_intent();
    let v: serde_json::Value =
        serde_json::to_value(intent.to_a2a_jsonrpc()).expect("A2A frame serializes to JSON");

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

/// The mailbox transport must be untouched: `Intent`'s own serialization stays the
/// flat native shape, so persisted rows and the cross-store pull format are
/// unchanged by the interop adapter.
#[test]
fn native_intent_wire_format_is_unchanged() {
    let v: serde_json::Value =
        serde_json::to_value(sample_intent()).expect("Intent serializes to JSON");
    assert_eq!(
        v.get("body").and_then(|b| b.as_str()),
        Some("build is green")
    );
    assert_eq!(v.get("from").and_then(|f| f.as_str()), Some("alice"));
    assert!(
        v.get("kind").is_none() && v.get("parts").is_none() && v.get("jsonrpc").is_none(),
        "the A2A adapter must not leak into Intent's native wire format"
    );
}

/// Round-tripping through A2A preserves the core fields.
#[test]
fn intent_round_trips_through_a2a() {
    let original = sample_intent();
    let back = Intent::from_a2a(&original.to_a2a());
    assert_eq!(back.id, original.id);
    assert_eq!(back.from, original.from);
    assert_eq!(back.to, original.to);
    assert_eq!(back.subject, original.subject);
    assert_eq!(back.body, original.body);
    assert_eq!(back.trace_id, original.trace_id);
    assert_eq!(back.idempotency_key, original.idempotency_key);
}
