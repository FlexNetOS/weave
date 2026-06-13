//! Blocking LLM client for thread summarization (WL-033).
//!
//! Feature-gated behind `llm` because it pulls in `reqwest`.

use crate::config::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Hard cap on characters of thread text sent to the LLM.
const MAX_INPUT_CHARS_HARD: usize = 16_000;
/// Hard cap on `max_tokens` in the completion request.
const MAX_TOKENS_HARD: u32 = 512;
/// Default timeout for a single LLM request.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

/// Summarize the provided thread text using the configured LLM endpoint.
///
/// Returns the summary text, or an error if the LLM is unconfigured or the
/// request fails.
pub fn summarize_text(config: &Config, thread_text: &str) -> Result<String> {
    let endpoint = if let Some(ref e) = config.llm_endpoint {
        e.clone()
    } else if let Ok(e) = std::env::var("WEAVE_LLM_ENDPOINT") {
        e
    } else {
        anyhow::bail!(
            "LLM endpoint not configured (set llm_endpoint in config or WEAVE_LLM_ENDPOINT)"
        );
    };

    let api_key = if let Some(ref k) = config.llm_api_key {
        k.clone()
    } else if let Ok(k) = std::env::var("WEAVE_LLM_API_KEY") {
        k
    } else {
        anyhow::bail!(
            "LLM API key not configured (set llm_api_key in config or WEAVE_LLM_API_KEY)"
        );
    };

    let model = config
        .llm_model
        .clone()
        .or_else(|| std::env::var("WEAVE_LLM_MODEL").ok())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let timeout_secs = config
        .llm_timeout_secs
        .map(|n| n.max(1) as u64)
        .or_else(|| {
            std::env::var("WEAVE_LLM_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let max_input = config
        .llm_max_input_chars
        .map(|n| (n as usize).min(MAX_INPUT_CHARS_HARD))
        .unwrap_or(MAX_INPUT_CHARS_HARD);

    let capped_text = if thread_text.len() > max_input {
        &thread_text[..max_input]
    } else {
        thread_text
    };

    let prompt = format!(
        "Summarize the following conversation thread concisely (one paragraph):\n\n{}",
        capped_text
    );

    let body = ChatRequest {
        model,
        messages: vec![Message {
            role: "user".to_string(),
            content: prompt,
        }],
        max_tokens: MAX_TOKENS_HARD,
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("building reqwest client")?;

    let resp = client
        .post(endpoint)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("sending LLM request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("LLM request failed: {} — {}", status, text);
    }

    let chat_resp: ChatResponse = resp.json().context("parsing LLM response")?;
    let summary = chat_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_endpoint_errors() {
        let cfg = Config::default();
        let err = summarize_text(&cfg, "hello").unwrap_err().to_string();
        assert!(err.contains("LLM endpoint not configured"), "{err}");
    }

    #[test]
    fn secret_redacted_in_debug() {
        let cfg = Config {
            llm_endpoint: Some("https://example.com".to_string()),
            llm_api_key: Some("secret123".to_string()),
            ..Config::default()
        };
        let dbg = format!("{:?}", cfg);
        assert!(
            !dbg.contains("secret123"),
            "api key must be redacted: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "api key must show <redacted>: {dbg}"
        );
    }
}
