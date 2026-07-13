//! Blocking LLM client for thread summarization (WL-033).
//!
//! Feature-gated behind `llm` because it pulls in `reqwest`.

use crate::config::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;

/// Hard cap on characters of thread text sent to the LLM.
const MAX_INPUT_CHARS_HARD: usize = 16_000;
/// Hard cap on raw provider response bytes read before JSON decoding.
const MAX_RESPONSE_BYTES_HARD: usize = 64 * 1024;
/// Hard cap on Unicode scalar values in a provider or cached summary.
const MAX_SUMMARY_CHARS_HARD: usize = 16_000;
/// Hard cap on `max_tokens` in the completion request.
const MAX_TOKENS_HARD: u32 = 512;
/// Default timeout for a single LLM request.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard timeout ceiling: a bad config must not pin a CLI/MCP request indefinitely.
const MAX_TIMEOUT_SECS: u64 = 300;
/// Model used when neither config nor its environment overlay selects one.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

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

/// The exact model name sent to the provider and persisted with cached thread
/// summaries. Keeping this resolver in the client prevents CLI/MCP cache metadata
/// from drifting to an "unknown" placeholder when the default model is in use.
pub fn effective_model(config: &Config) -> &str {
    config
        .llm_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_MODEL)
}

/// Validate and normalize provider or cached summary text before it can be
/// persisted or rendered. Output is one paragraph, bounded by Unicode scalar
/// count, and contains no non-whitespace control characters (including ANSI ESC).
pub fn normalize_summary_text(summary: &str) -> Result<String> {
    if summary.chars().take(MAX_SUMMARY_CHARS_HARD + 1).count() > MAX_SUMMARY_CHARS_HARD {
        anyhow::bail!(
            "LLM summary exceeds the {MAX_SUMMARY_CHARS_HARD}-character Unicode-scalar limit"
        );
    }

    let mut normalized = String::with_capacity(summary.len());
    let mut pending_space = false;
    for ch in summary.chars() {
        if ch.is_whitespace() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if ch.is_control() {
            anyhow::bail!("LLM summary contains unsafe non-whitespace control characters");
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(ch);
    }

    if normalized.is_empty() {
        anyhow::bail!("LLM provider returned an empty summary");
    }
    Ok(normalized)
}

/// Summarize the provided thread text using the configured LLM endpoint.
///
/// Returns the summary text, or an error if the LLM is unconfigured or the
/// request fails.
pub fn summarize_text(config: &Config, thread_text: &str) -> Result<String> {
    let endpoint = config
        .llm_endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .context("LLM endpoint not configured (set llm_endpoint or WEAVE_LLM_ENDPOINT)")?;
    let api_key = config
        .llm_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .context("LLM API key not configured (set llm_api_key or WEAVE_LLM_API_KEY)")?;
    if thread_text.trim().is_empty() {
        anyhow::bail!("text to summarize must be non-empty");
    }

    let model = effective_model(config).to_string();

    let timeout_secs = config
        .llm_timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS as i64)
        .clamp(1, MAX_TIMEOUT_SECS as i64) as u64;

    let max_input = config
        .llm_max_input_chars
        .unwrap_or(MAX_INPUT_CHARS_HARD as i64)
        .clamp(1, MAX_INPUT_CHARS_HARD as i64) as usize;
    let capped_text: String = thread_text.chars().take(max_input).collect();

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
        // The configured provider is the complete outbound trust boundary. Do
        // not let it redirect credentials or thread content to another origin
        // (including a downgraded or internal-network URL).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building reqwest client")?;

    let resp = client
        .post(endpoint)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|err| anyhow::anyhow!("sending LLM request: {}", err.without_url()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("LLM request failed with HTTP {status}");
    }

    let mut response_bytes = Vec::with_capacity(MAX_RESPONSE_BYTES_HARD.min(8 * 1024));
    resp.take((MAX_RESPONSE_BYTES_HARD + 1) as u64)
        .read_to_end(&mut response_bytes)
        .map_err(|_| anyhow::anyhow!("reading LLM response"))?;
    if response_bytes.len() > MAX_RESPONSE_BYTES_HARD {
        anyhow::bail!("LLM provider response exceeds the {MAX_RESPONSE_BYTES_HARD}-byte limit");
    }
    let chat_resp: ChatResponse =
        serde_json::from_slice(&response_bytes).context("parsing LLM response")?;
    let summary = chat_resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default();

    normalize_summary_text(&summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    fn accept_before(listener: &TcpListener, timeout: std::time::Duration) -> Option<TcpStream> {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Some(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("accept LLM request: {e}"),
            }
        }
    }

    /// Serve one deterministic OpenAI-compatible response on loopback and return
    /// the raw request for exact header/body assertions. No external network is
    /// involved in any LLM client test.
    fn serve_once(status: &str, response_body: &str) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback LLM fixture");
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let response_body = response_body.to_string();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut stream = accept_before(&listener, std::time::Duration::from_secs(5))
                .expect("LLM client did not connect to loopback fixture");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 2048];
            let header_end = loop {
                let n = stream.read(&mut buf).expect("read LLM request");
                assert!(n > 0, "client closed before request headers completed");
                request.extend_from_slice(&buf[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len() < header_end + content_len {
                let n = stream.read(&mut buf).expect("read LLM request body");
                assert!(n > 0, "client closed before request body completed");
                request.extend_from_slice(&buf[..n]);
            }
            tx.send(String::from_utf8(request).expect("request is UTF-8"))
                .unwrap();
            if let Err(err) = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            ) {
                assert!(
                    matches!(
                        err.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ),
                    "write LLM fixture response: {err}"
                );
            }
        });
        (format!("http://{addr}/v1/chat/completions"), rx, handle)
    }

    fn configured(endpoint: String) -> Config {
        Config {
            llm_endpoint: Some(endpoint),
            llm_api_key: Some("test-api-key".to_string()),
            llm_model: Some("fixture-model".to_string()),
            llm_timeout_secs: Some(2),
            ..Config::default()
        }
    }

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

    #[test]
    fn local_http_success_sends_openai_request_shape() {
        let (endpoint, request, server) = serve_once(
            "200 OK",
            r#"{"choices":[{"message":{"content":" concise result "}}]}"#,
        );
        let summary = summarize_text(&configured(endpoint), "alice: hello").unwrap();
        assert_eq!(summary, "concise result");

        let request = request.recv().unwrap();
        server.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-api-key\r\n"));
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["max_tokens"], MAX_TOKENS_HARD);
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("alice: hello"));
    }

    #[test]
    fn input_cap_counts_unicode_scalars_without_panicking() {
        let (endpoint, request, server) =
            serve_once("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut cfg = configured(endpoint);
        cfg.llm_max_input_chars = Some(2);
        assert_eq!(summarize_text(&cfg, "é🙂z").unwrap(), "ok");

        let request = request.recv().unwrap();
        server.join().unwrap();
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.ends_with("é🙂"), "capped prompt: {content:?}");
        assert!(!content.ends_with('z'), "third scalar must be capped");
    }

    #[test]
    fn https_scheme_is_supported_without_external_network() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback TLS fixture");
        let addr = listener.local_addr().unwrap();
        let (connected_tx, connected_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let connected = accept_before(&listener, std::time::Duration::from_secs(2)).is_some();
            connected_tx.send(connected).unwrap();
        });
        let mut cfg = configured(format!("https://{addr}/v1/chat/completions"));
        cfg.llm_timeout_secs = Some(1);
        let err = summarize_text(&cfg, "hello").unwrap_err();
        let detail = format!("{err:#}");
        let connected = connected_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("TLS fixture reported connection state");
        server.join().unwrap();
        assert!(
            connected,
            "TLS-enabled reqwest must open a loopback TCP connection before handshake failure: {detail}"
        );
        assert!(
            !detail.contains("scheme is not http"),
            "a TLS-enabled client must accept https before the loopback handshake fails: {detail}"
        );
    }

    #[test]
    fn input_cap_clamps_zero_to_one_and_large_values_to_hard_ceiling() {
        let (endpoint, request, server) =
            serve_once("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut cfg = configured(endpoint);
        cfg.llm_max_input_chars = Some(0);
        summarize_text(&cfg, "abc").unwrap();
        let request = request.recv().unwrap();
        server.join().unwrap();
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with('a'));

        let (endpoint, request, server) =
            serve_once("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut cfg = configured(endpoint);
        cfg.llm_max_input_chars = Some(i64::MAX);
        summarize_text(&cfg, &"z".repeat(MAX_INPUT_CHARS_HARD + 1)).unwrap();
        let request = request.recv().unwrap();
        server.join().unwrap();
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        let content = body["messages"][0]["content"].as_str().unwrap();
        let suffix = content.split_once("\n\n").unwrap().1;
        assert_eq!(suffix.chars().count(), MAX_INPUT_CHARS_HARD);
    }

    #[test]
    fn empty_provider_summary_is_an_error() {
        let (endpoint, _request, server) =
            serve_once("200 OK", r#"{"choices":[{"message":{"content":"   "}}]}"#);
        let err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(err.contains("empty summary"), "clear error: {err}");
    }

    #[test]
    fn upstream_error_body_and_api_key_are_not_echoed() {
        let leaked = "test-api-key provider-private-detail ".repeat(2_000);
        let (endpoint, _request, server) = serve_once("500 Internal Server Error", &leaked);
        let err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(
            !err.contains("test-api-key"),
            "API key leaked in {}-byte error",
            err.len()
        );
        assert!(
            !err.contains("provider-private-detail"),
            "upstream response body leaked in {}-byte error",
            err.len()
        );
        assert!(
            err.len() < 256,
            "error must stay bounded, got {} bytes",
            err.len()
        );
    }

    #[test]
    fn provider_response_body_is_bounded_before_json_decode() {
        let private = "provider-private-oversized-detail";
        let body = serde_json::json!({
            "choices": [{"message": {"content": private.repeat(3_000)}}]
        })
        .to_string();
        assert!(body.len() > 64 * 1024, "fixture must exceed the hard cap");
        let (endpoint, _request, server) = serve_once("200 OK", &body);
        let err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(err.contains("65536"), "clear response cap error: {err}");
        assert!(!err.contains(private), "provider body leaked: {err}");
        assert!(!err.contains("test-api-key"), "API key leaked: {err}");
    }

    #[test]
    fn oversized_summary_is_rejected_by_unicode_scalar_count() {
        let content = "é".repeat(16_001);
        let body = serde_json::json!({
            "choices": [{"message": {"content": content}}]
        })
        .to_string();
        assert!(
            body.len() < 64 * 1024,
            "fixture must reach summary validation"
        );
        let (endpoint, _request, server) = serve_once("200 OK", &body);
        let err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(err.contains("16000"), "clear summary cap error: {err}");
    }

    #[test]
    fn summary_whitespace_is_collapsed_and_controls_are_rejected() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "  first\n\tsecond\r\nthird\u{2003}fourth  "}}]
        })
        .to_string();
        let (endpoint, _request, server) = serve_once("200 OK", &body);
        let summary = summarize_text(&configured(endpoint), "hello").unwrap();
        server.join().unwrap();
        assert_eq!(summary, "first second third fourth");

        let body = serde_json::json!({
            "choices": [{"message": {"content": "safe\u{1b}[31munsafe"}}]
        })
        .to_string();
        let (endpoint, _request, server) = serve_once("200 OK", &body);
        let err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(err.contains("control"), "clear unsafe-output error: {err}");
        assert!(!err.contains("[31munsafe"), "provider output leaked: {err}");
    }

    #[test]
    fn request_and_decode_errors_omit_endpoint_canaries() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let send_canary = "send-path-canary";
        let endpoint = format!("http://{addr}/{send_canary}?secret={send_canary}");
        let send_err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        assert!(!send_err.contains(send_canary), "URL leaked: {send_err}");
        assert!(
            !send_err.contains("test-api-key"),
            "API key leaked: {send_err}"
        );

        let (endpoint, _request, server) = serve_once("200 OK", "{malformed-json");
        let decode_canary = "decode-path-canary";
        let endpoint = endpoint.replace(
            "/v1/chat/completions",
            &format!("/{decode_canary}?secret={decode_canary}"),
        );
        let decode_err = summarize_text(&configured(endpoint), "hello")
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(decode_err.contains("parsing LLM response"), "{decode_err}");
        assert!(
            !decode_err.contains(decode_canary),
            "URL leaked: {decode_err}"
        );
        assert!(
            !decode_err.contains("test-api-key"),
            "API key leaked: {decode_err}"
        );
    }

    #[test]
    fn provider_redirects_are_not_followed_or_disclosed() {
        let redirect_listener =
            TcpListener::bind("127.0.0.1:0").expect("bind redirecting provider");
        let redirect_addr = redirect_listener.local_addr().unwrap();
        let target_listener = TcpListener::bind("127.0.0.1:0").expect("bind redirect target");
        let target_addr = target_listener.local_addr().unwrap();
        let location_canary = "redirect-location-canary";

        let redirect_server = std::thread::spawn(move || {
            let mut stream = accept_before(&redirect_listener, std::time::Duration::from_secs(2))
                .expect("LLM client did not connect to redirecting provider");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).expect("read redirect request");
                assert!(n > 0, "client closed before redirect request completed");
                request.extend_from_slice(&buf[..n]);
            }
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/{location_canary}?secret={location_canary}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("write redirect response");
        });
        let (target_tx, target_rx) = mpsc::channel();
        let target_server = std::thread::spawn(move || {
            target_tx
                .send(
                    accept_before(&target_listener, std::time::Duration::from_millis(500))
                        .is_some(),
                )
                .unwrap();
        });

        let endpoint_canary = "redirect-provider-path-canary";
        let endpoint = format!("http://{redirect_addr}/{endpoint_canary}?secret={endpoint_canary}");
        let err = summarize_text(&configured(endpoint), "private thread body")
            .unwrap_err()
            .to_string();
        redirect_server.join().unwrap();
        let target_connected = target_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("redirect target reported connection state");
        target_server.join().unwrap();

        assert!(
            err.contains("HTTP 302"),
            "redirect is a status error: {err}"
        );
        assert!(
            !target_connected,
            "provider redirect target must not be contacted"
        );
        assert!(!err.contains(endpoint_canary), "provider URL leaked: {err}");
        assert!(!err.contains(location_canary), "Location URL leaked: {err}");
        assert!(!err.contains("test-api-key"), "API key leaked: {err}");
    }
}
