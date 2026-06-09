# Plan — WL-033: Thread summarization via LLM integration (mcp_agent_mail parity)

> **Status:** Core implementation is already landed in the codebase (model, store trait + both backends, LLM module, config, CLI flags, MCP tools). This plan covers the **remaining gaps**: tests, docs, and verification.

## Goal
Enable LLM-powered thread summarization in weave, with summaries cached in the store, exposed via MCP tools and CLI, backed by a lightweight `reqwest::blocking` client gated behind the `llm` feature.

## Architecture (already implemented)

- **`weave-core/src/llm.rs`** — minimal blocking HTTP client for OpenAI-compatible chat-completion APIs. Uses `reqwest::blocking` with `json` feature. Gated behind `llm` Cargo feature.
- **`weave-core/src/model.rs`** — `Summary` struct with `Serialize/Deserialize`.
- **`weave-core/src/store.rs` / `store_libsql.rs`** — `summaries` table + `store_summary` / `get_summary` / `delete_summary` methods in both backends.
- **`weave-core/src/config.rs`** — `llm_endpoint`, `llm_api_key`, `llm_model`, `llm_timeout_secs`, `llm_max_input_chars`, all env-overlayed and secret-redacted in `Debug`.
- **`weave/src/main.rs`** — `Thread { --summarize, --refresh }` and `Summarize { text }` CLI subcommands.
- **`weave-mcp/src/mcp.rs`** — `weave_thread_summarize` and `weave_summarize_text` registered in `tools()` and dispatched in `call_tool()`.
- **Feature wiring** — `llm` feature on `weave-core`, `weave-mcp`, and `weave` crates; default OFF.

## Remaining work

### 1. Store unit tests (both backends)

Add tests in `weave-core/src/store.rs` under `#[cfg(all(test, feature = "sqlite"))]` and corresponding libsql tests (if a libsql test module exists):

- `summary_roundtrip`: `store_summary` → `get_summary` returns correct `Summary` (id, root_id, text, model, created_ts, refreshed_ts).
- `summary_upsert_refresh`: calling `store_summary` twice with the same `root_id` updates `text`, `model`, and `refreshed_ts` while preserving `created_ts`.
- `summary_delete`: `delete_summary` returns `1`, subsequent `get_summary` returns `None`.
- `summary_unknown_root`: `get_summary` on a non-existent `root_id` returns `None`.

Use the existing `mem()` helper for sqlite tests; mirror the pattern for libsql if a test harness exists.

### 2. LLM module unit tests (already partially present — verify completeness)

Existing tests in `weave-core/src/llm.rs`:
- `render_thread_caps_input` ✓
- `build_chat_request_shape` ✓
- `parse_chat_response_ok` ✓
- `parse_chat_response_missing_choices` ✓
- `parse_chat_response_empty_content` ✓
- `llm_params_clamping` ✓

**Add:**
- `summarize_text_unconfigured_errors`: when `endpoint` or `api_key` is `None`, returns a clean `Err` (no panic, no network attempt).
- `params_from_config_redacts_key`: `LlmParams` cloned from config does not expose the API key via `Debug`.

### 3. Integration tests (`weave/tests/integration.rs`)

- **`cli_thread_summarize_unconfigured`**: run `weave thread --root 1 --summarize` with no LLM config → stderr contains "not configured" and exit code is `1`.
- **`cli_summarize_text_unconfigured`**: run `weave summarize --text "hello"` with no LLM config → stderr contains "not configured" and exit code is `1`.
- **`cli_thread_summarize_json`**: with LLM configured (use a mock/wiremock HTTP server or stub if possible; if not, at least verify JSON shape when cached):
  - First call: returns `{"summary": "...", "cached": false, "root_id": 1}`.
  - Second call (no `--refresh`): returns `{"summary": "...", "cached": true, "root_id": 1}`.
- **`cli_thread_summarize_refresh`**: `--refresh` bypasses cache and re-fetches.
- **`mcp_weave_thread_summarize_tool_exists`**: `tools/list` response contains `weave_thread_summarize` and `weave_summarize_text`.
- **`mcp_weave_thread_summarize_unconfigured`**: MCP call returns `isError: true` with a clear message when LLM is unconfigured.
- **`mcp_weave_summarize_text_unconfigured`**: same for ad-hoc text summarization.

> **Note:** If adding a real HTTP mock server is too heavy, integration tests can verify the "unconfigured" error path and the MCP tool registry presence. The happy path with a mock LLM server is desirable but optional if unit tests cover `summarize_text` logic.

### 4. Security / invariant verification

- API key redaction: `Config` Debug output shows `<redacted>` for `llm_api_key`. Assert this in a unit test.
- Input capping: `render_thread` and `summarize_text` both truncate oversized input before building the HTTP body. Assert the truncated length never exceeds `max_input_chars`.
- Timeout clamping: `llm_timeout_secs` of `0` or `999_999` clamps to `[5, 120]`.
- No shell invocation: LLM path uses only `reqwest::blocking::Client::post()`; no `Command::new` or `std::process`.
- No secrets in logs: `llm::summarize_text` errors must never include the `api_key` in the error string.

### 5. Dual-backend gate

Run and pass:
```bash
cargo fmt --check
cargo clippy -D warnings --features sqlite,llm
cargo clippy -D warnings --no-default-features --features libsql,llm
cargo test --features sqlite,llm
cargo test --no-default-features --features libsql,llm
```

### 6. Docs updates

- **`CHANGELOG.md`** — add `[Unreleased]` entry noting:
  - New MCP tools: `weave_thread_summarize`, `weave_summarize_text`
  - New CLI flags: `weave thread --summarize --refresh`, `weave summarize --text`
  - New config keys: `llm_endpoint`, `llm_api_key`, `llm_model`, `llm_timeout_secs`, `llm_max_input_chars`
  - Gated behind `--features llm`
- **`ARCHITECTURE.md`** — document:
  - `llm` module location and feature gate
  - Config/env secret handling (redaction, prefer env var over config file)
  - Prompt rendering + input cap discipline
  - Summary caching strategy (`SUMMARY_CACHE_TTL_SECS = 3600`)
- **`README.md`** — list new MCP tools and CLI commands in the feature table; note `llm` feature requirement.
- **`docs/OPERATIONS.md`** — document the five config keys and env overrides; recommend `WEAVE_LLM_API_KEY` over `config.toml`.

## Invariant checklist (weave-invariants skill)

- [ ] **No shellouts** — LLM call is HTTP-only (`reqwest::blocking`).
- [ ] **Argv-only spawning** — N/A for this feature (no new processes).
- [ ] **Parameterized SQL** — `store_summary` / `get_summary` / `delete_summary` use `params!` / `libsql::Value` binds.
- [ ] **Strict module layering** — `llm` lives in `weave-core`; `weave-mcp` and `weave` bin consume it. No upward deps.
- [ ] **Paste-safe injection** — N/A (summarization never injects text into panes).
- [ ] **Input caps** — `llm_max_input_chars`, `MAX_TOKENS`, timeout clamps all enforced.
- [ ] **Destructive-op gating** — N/A (no destructive ops; `delete_summary` is test-only surface).
- [ ] **MCP stdout discipline** — LLM errors are returned as `isError: true` text content, never logged to stdout.

## Acceptance criteria

1. `cargo test --features sqlite,llm` passes, including new store + LLM unit tests.
2. `cargo test --no-default-features --features libsql,llm` passes (store tests).
3. Integration tests cover at minimum the "unconfigured" error paths and MCP tool registry presence.
4. `cargo clippy -D warnings` clean for both feature columns.
5. CHANGELOG, ARCHITECTURE, README, and OPERATIONS updated.
