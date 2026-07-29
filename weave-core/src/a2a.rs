//! A2A (Agent2Agent) v1.0 interop adapter — strict-upgrade, default-off.
//!
//! weave carries agent-to-agent traffic using its own [`Intent`] wire schema over a
//! SQLite-mailbox + HTTP-push substrate. This module ADDS the Linux-Foundation A2A
//! v1.0 interop shape alongside it: a `Message` object (`kind`/`role`/`messageId`/
//! `parts[]`) and the JSON-RPC 2.0 `message/send` transport envelope.
//!
//! It is deliberately a *separate* representation rather than a change to
//! `Intent`'s own serde derive. `Intent` is the persisted mailbox row and the
//! cross-store pull format; re-shaping its serialization would rewrite the
//! on-disk/on-the-wire contract of the existing transport. Interop is additive:
//! callers that want A2A ask for it via [`Intent::to_a2a`] /
//! [`Intent::to_a2a_jsonrpc`], and inbound A2A arrives through
//! [`Intent::from_a2a`].
//!
//! Enable with the default-off `a2a` feature.

use serde::{Deserialize, Serialize};

use crate::model::Intent;

/// A2A `Message.role`. weave emits agent-originated traffic.
pub const ROLE_AGENT: &str = "agent";
/// A2A `Message.kind` discriminator.
pub const KIND_MESSAGE: &str = "message";
/// A2A `Part.kind` for a text part.
pub const PART_KIND_TEXT: &str = "text";
/// A2A JSON-RPC method for an outbound send.
pub const METHOD_MESSAGE_SEND: &str = "message/send";
/// JSON-RPC protocol version.
pub const JSONRPC_VERSION: &str = "2.0";

/// One A2A `Part`. Only text parts are produced today; the `kind` tag keeps the
/// door open for file/data parts without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aPart {
    pub kind: String,
    pub text: String,
}

impl A2aPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: PART_KIND_TEXT.to_string(),
            text: text.into(),
        }
    }
}

/// weave routing carried in the A2A `Message.metadata` bag. A2A leaves metadata
/// free-form, so this is where the mailbox addressing that A2A has no field for
/// travels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aMetadata {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,
}

/// An A2A v1.0 `Message` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aMessage {
    pub kind: String,
    pub role: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub parts: Vec<A2aPart>,
    #[serde(default)]
    pub metadata: A2aMetadata,
}

/// `params` of an A2A `message/send` JSON-RPC request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aSendParams {
    pub message: A2aMessage,
}

/// An A2A v1.0 outbound frame: JSON-RPC 2.0 `message/send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aJsonRpcRequest {
    pub jsonrpc: String,
    pub id: i64,
    pub method: String,
    pub params: A2aSendParams,
}

impl Intent {
    /// Map this intent onto an A2A v1.0 `Message` object.
    ///
    /// `id -> messageId`, `body -> parts[0].text`, and the mailbox addressing that
    /// A2A has no first-class field for travels in `metadata`.
    pub fn to_a2a(&self) -> A2aMessage {
        A2aMessage {
            kind: KIND_MESSAGE.to_string(),
            role: ROLE_AGENT.to_string(),
            message_id: self.id.to_string(),
            parts: vec![A2aPart::text(self.body.clone())],
            metadata: A2aMetadata {
                from: self.from.clone(),
                to: self.to.clone(),
                subject: self.subject.clone(),
                to_host: self.to_host.clone(),
                trace_id: self.trace_id.clone(),
                idempotency_key: self.idempotency_key.clone(),
                priority: self.priority.clone(),
            },
        }
    }

    /// Frame this intent as an A2A v1.0 JSON-RPC 2.0 `message/send` request.
    pub fn to_a2a_jsonrpc(&self) -> A2aJsonRpcRequest {
        A2aJsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: self.id,
            method: METHOD_MESSAGE_SEND.to_string(),
            params: A2aSendParams {
                message: self.to_a2a(),
            },
        }
    }

    /// Parse an inbound A2A v1.0 `Message` into a weave [`Intent`].
    ///
    /// `messageId` is advisory on the way in: a non-numeric id from a foreign
    /// agent yields `id = 0` and the receiver assigns its own on commit, exactly
    /// as it re-stamps `ts`. All text parts are joined so a multi-part message
    /// does not silently lose content.
    pub fn from_a2a(msg: &A2aMessage) -> Self {
        let body = msg
            .parts
            .iter()
            .filter(|p| p.kind == PART_KIND_TEXT)
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Intent {
            id: msg.message_id.parse::<i64>().unwrap_or(0),
            ts: 0,
            to: msg.metadata.to.clone(),
            to_host: msg.metadata.to_host.clone(),
            from: msg.metadata.from.clone(),
            subject: msg.metadata.subject.clone(),
            body,
            sig: String::new(),
            idempotency_key: msg.metadata.idempotency_key.clone(),
            trace_id: msg.metadata.trace_id.clone(),
            priority: if msg.metadata.priority.is_empty() {
                "normal".to_string()
            } else {
                msg.metadata.priority.clone()
            },
            ttl: 0,
        }
    }
}
