//! Minimal, blocking HTTP client for OpenAI-compatible chat-completion APIs (WL-033).
//!
//! Lives in `weave-core` so both `weave-mcp` and the `weave` bin can use it
//! without introducing a new crate or upward dependency. The `llm` Cargo
//! feature gates the `reqwest` dependency; when disabled the public API still
//! exists but returns clean errors so callers compile unchanged.

use crate::config::Config;
use crate::model::Message;
use anyhow::Result;

/// Default OpenAI-compatible chat-completion endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
/// Default model when none is configured.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
/// Default connect+read timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default input-character cap before the request body is built.
pub const DEFAULT_MAX_INPUT_CHARS: usize = 32_768;
/// Hard floor for timeout (seconds).
pub const MIN_TIMEOUT_SECS: u64 = 5;
/// Hard ceiling for timeout (seconds).
pub const MAX_TIMEOUT_SECS: u64 = 120;
/// Hard floor for max-input-chars.
pub const MIN_MAX_INPUT_CHARS: usize = 1_024;
/// Hard ceiling for max-input-chars.
pub const MAX_MAX_INPUT_CHARS: usize = 65_536;
/// Hard ceiling on `max_tokens` in the LLM request (bounds response size).
pub const MAX_TOKENS: u32 = 512;
/// Cache-ttl: a summary younger than this is considered fresh (seconds).
pub const SUMMARY_CACHE_TTL_SECS: i64 = 3_600;

/// The subset of [`Config`] needed for LLM calls. Small, cloneable, and secret-
/// redacted in [`std::fmt::Debug`] so it never leaks the API key via logs.
#[derive(Clone)]
pub struct LlmParams {
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_input_chars: Option<usize>,
}

impl std::fmt::Debug for LlmParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmParams")
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_input_chars", &self.max_input_chars)
            .finish()
    }
}

impl LlmParams {
    /// True when both endpoint and api_key are present (the minimum needed to
    /// make a real LLM call).
    pub fn is_configured(&self) -> bool {
        self.endpoint.is_some() && self.api_key.is_some()
    }

    /// Resolved endpoint (default when unset).
    pub fn endpoint(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
    }

    /// Resolved model (default when unset).
    pub fn model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// Resolved timeout, clamped to [`MIN_TIMEOUT_SECS`, `MAX_TIMEOUT_SECS`].
    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
    }

    /// Resolved max-input-chars, clamped to
    /// [`MIN_MAX_INPUT_CHARS`, `MAX_MAX_INPUT_CHARS`].
    pub fn max_input_chars(&self) -> usize {
        self.max_input_chars
            .unwrap_or(DEFAULT_MAX_INPUT_CHARS)
            .clamp(MIN_MAX_INPUT_CHARS, MAX_MAX_INPUT_CHARS)
    }
}

/// Build the LLM params from a [`Config`].
pub fn params_from_config(cfg: &Config) -> LlmParams {
    LlmParams {
        endpoint: cfg.llm_endpoint.clone(),
        api_key: cfg.llm_api_key.clone(),
        model: cfg.llm_model.clone(),
        timeout_secs: cfg.llm_timeout_secs,
        max_input_chars: cfg.llm_max_input_chars,
    }
}

/// Render a thread oldest-first as `"{sender}: {body}\n"`, capping to
/// `max_chars`. Lossy-but-total: oversized input is silently truncated.
pub fn render_thread(messages: &[Message], max_chars: usize) -> String {
    let mut out = String::new();
    for m in messages {
        let line = format!("{}: {}\n", m.sender, m.body);
        if out.len() + line.len() > max_chars {
            let remaining = max_chars.saturating_sub(out.len());
            if remaining > 0 {
                // Best-effort: take a prefix of the last line so we never
                // exceed the cap, but never panic on a char boundary.
                let safe = &line[..line.ceil_char_boundary(remaining)];
                out.push_str(safe);
            }
            break;
        }
        out.push_str(&line);
    }
    out
}

/// Summarize arbitrary text via the configured LLM endpoint.
///
/// Errors cleanly when:
/// - the `llm` feature is not compiled in,
/// - no endpoint / api_key is configured,
/// - the HTTP call fails or times out,
/// - the response JSON is malformed or missing `choices[0].message.content`.
pub fn summarize_text(params: &LlmParams, text: &str) -> Result<String> {
    if !params.is_configured() {
        anyhow::bail!("LLM not configured (set llm_endpoint and llm_api_key).");
    }

    let max_chars = params.max_input_chars();
    let capped = if text.len() > max_chars {
        &text[..text.floor_char_boundary(max_chars)]
    } else {
        text
    };

    #[cfg(feature = "llm")]
    {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(params.timeout_secs()))
            .timeout(std::time::Duration::from_secs(params.timeout_secs()))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;

        let body = build_chat_request(&params.model(), capped);
        let resp = client
            .post(params.endpoint())
            .header(
                "Authorization",
                format!("Bearer {}", params.api_key.as_deref().unwrap_or("")),
            )
            .json(&body)
            .send()
            .map_err(|e| anyhow::anyhow!("LLM request failed: {e}"))?;

        let status = resp.status();
        let text_body = resp
            .text()
            .map_err(|e| anyhow::anyhow!("failed to read LLM response body: {e}"))?;
        if !status.is_success() {
            anyhow::bail!("LLM returned HTTP {status}");
        }
        parse_chat_response(&text_body)
    }

    #[cfg(not(feature = "llm"))]
    {
        let _ = capped;
        anyhow::bail!("LLM support not compiled in (enable the `llm` feature).")
    }
}

/// Build the standard OpenAI chat-completion request JSON.
fn build_chat_request(model: &str, user_text: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "Summarize the following conversation thread concisely."},
            {"role": "user", "content": user_text}
        ],
        "max_tokens": MAX_TOKENS
    })
}

/// Parse the chat-completion response, extracting `choices[0].message.content`.
fn parse_chat_response(body: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("LLM response is invalid JSON: {e}"))?;
    let content = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("LLM response missing content field"))?;
    if content.trim().is_empty() {
        anyhow::bail!("LLM returned empty summary");
    }
    Ok(content.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_thread_caps_input() {
        let msgs = vec![
            Message {
                id: 1,
                ts: 0,
                sender: "a".into(),
                recipient: "b".into(),
                subject: None,
                body: "hello".into(),
                in_reply_to: None,
            },
            Message {
                id: 2,
                ts: 0,
                sender: "b".into(),
                recipient: "a".into(),
                subject: None,
                body: "world".into(),
                in_reply_to: None,
            },
        ];
        let out = render_thread(&msgs, 100);
        assert!(out.contains("a: hello"));
        assert!(out.contains("b: world"));

        let short = render_thread(&msgs, 5);
        assert!(short.len() <= 5, "expected <=5, got {}", short.len());
    }

    #[test]
    fn build_chat_request_shape() {
        let req = build_chat_request("gpt-test", "some text");
        assert_eq!(req["model"], "gpt-test");
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(req["max_tokens"], MAX_TOKENS);
    }

    #[test]
    fn parse_chat_response_ok() {
        let json = r#"{"choices":[{"message":{"content":"  A summary.  "}}]}"#;
        assert_eq!(parse_chat_response(json).unwrap(), "A summary.");
    }

    #[test]
    fn parse_chat_response_missing_choices() {
        let json = r#"{"choices":[]}"#;
        assert!(parse_chat_response(json).is_err());
    }

    #[test]
    fn parse_chat_response_empty_content() {
        let json = r#"{"choices":[{"message":{"content":"   "}}]}"#;
        assert!(parse_chat_response(json).is_err());
    }

    #[test]
    fn llm_params_clamping() {
        let p = LlmParams {
            endpoint: None,
            api_key: None,
            model: None,
            timeout_secs: Some(0),
            max_input_chars: Some(999_999),
        };
        assert_eq!(p.timeout_secs(), MIN_TIMEOUT_SECS);
        assert_eq!(p.max_input_chars(), MAX_MAX_INPUT_CHARS);
    }
}
