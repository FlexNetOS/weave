# Implementer Changes — WL-033: Thread summarization via LLM integration

## Summary
Added LLM-powered thread summarization to weave, persisting summaries in the store and exposing them via MCP tools and CLI. The feature is gated behind the new `llm` Cargo feature (default OFF) so the standard static binary gains no new network dependency.

## Files Modified/Created

### weave-core
- **`Cargo.toml`** — Added `llm` feature (`["dep:reqwest"]`) and optional `reqwest` dependency with `json` + `blocking` features.
- **`src/lib.rs`** — Added `pub mod llm;`.
- **`src/llm.rs`** — **NEW.** Minimal blocking HTTP client for OpenAI-compatible chat-completion APIs. Exposes:
  - `LlmParams` (cloneable, secret-redacted Debug)
  - `params_from_config(&Config) -> LlmParams`
  - `is_configured()`, `endpoint()`, `model()`, `timeout_secs()`, `max_input_chars()`
  - `render_thread(&[Message], max_chars) -> String`
  - `summarize_text(&LlmParams, &str) -> Result<String>`
  - When `llm` feature is OFF, `summarize_text` returns a clean error; all other helpers work unconditionally.
  - Unit tests for rendering, request shape, response parsing, and parameter clamping.
- **`src/model.rs`** — Added `Summary` struct (`id`, `root_id`, `text`, `model`, `created_ts`, `refreshed_ts`).
- **`src/config.rs`** — Added five new optional fields (`llm_endpoint`, `llm_api_key`, `llm_model`, `llm_timeout_secs`, `llm_max_input_chars`) with env overlays (`WEAVE_LLM_*`), Debug redaction for `llm_api_key`, and clamping accessors. Updated `CONFIG_TEMPLATE` and its drift-guard tests.
- **`src/store.rs`** — Added `summaries` table + index to `SCHEMA`, idempotent migration in `migrate()`, and three new `Store` trait methods:
  - `store_summary(root_id, text, model) -> Result<i64>` (upsert via `ON CONFLICT(root_id) DO UPDATE`)
  - `get_summary(root_id) -> Result<Option<Summary>>`
  - `delete_summary(root_id) -> Result<usize>`
  - Implemented all three for `SqliteStore`.
- **`src/store_libsql.rs`** — Mirrored every store change for `LibsqlStore`:
  - Added `summaries` table + index to `SCHEMA` array.
  - Added idempotent migration in `open()`.
  - Implemented `store_summary`, `get_summary`, `delete_summary` using `block_on`.

### weave-mcp
- **`Cargo.toml`** — Added `llm = ["weave-core/llm"]` feature.
- **`src/mcp.rs`** —
  - Updated `serve()` signature to accept `llm_params: llm::LlmParams`.
  - Threaded `llm_params` through `handle()` → `call_tool()`.
  - Added two new MCP tools to `tools()`:
    - `weave_thread_summarize` — cached thread summarization with `refresh` flag.
    - `weave_summarize_text` — ad-hoc text summarization with no persistence.
  - Added `tool_thread_summarize()` and `tool_summarize_text()` implementations.

### weave (bin)
- **`Cargo.toml`** — Added `llm = ["weave-core/llm", "weave-mcp/llm"]` feature.
- **`src/main.rs`** —
  - Added `llm` to the `weave_core` import list.
  - Added `--summarize` and `--refresh` flags to the `Thread` CLI subcommand.
  - Added new `Summarize { text, json }` top-level subcommand.
  - Threaded `llm::params_from_config(&cfg)` into `mcp::serve()`.
  - Implemented CLI dispatch for summarization paths (cache check, thread render, LLM call, persistence, JSON/plain output). Exits 1 with a one-line error when LLM is unconfigured.

## Noteworthy Decisions

1. **Unconditional `llm` module, gated `reqwest` dependency.** The `llm` module is always present in `weave-core` so that `weave-mcp` and `weave` bin compile unchanged regardless of the `llm` feature. Only the HTTP client inside `summarize_text` is `#[cfg(feature = "llm")]`-gated; without the feature it returns a clean, user-facing error. This avoids sprinkling `#[cfg(feature = "llm")]` across MCP tool tables and CLI dispatch logic.

2. **Store trait methods are unconditional.** `store_summary`, `get_summary`, and `delete_summary` live on the `Store` trait and are implemented in both backends unconditionally. The persistence layer has no dependency on `reqwest`.

3. **Secret discipline.** `llm_api_key` is redacted in both `Config` Debug and `LlmParams` Debug. It is bound as an `Authorization: Bearer` header and never interpolated into logs, errors, or stdout.

4. **Input caps.** `llm_timeout_secs` clamps to `[5, 120]` and `llm_max_input_chars` clamps to `[1_024, 65_536]`. Thread text is truncated lossily-but-total before the request body is built. `max_tokens` is hard-coded to 512.

5. **Caching.** `get_summary` + age check (`SUMMARY_CACHE_TTL_SECS = 3600`) provides the default caching behavior; `--refresh` / `refresh=true` bypasses it.

6. **No shellouts.** All external interaction is via `reqwest::blocking` (pure HTTP). No `std::process::Command` is used in the LLM path.

## Build Verification

- `cargo build` (default sqlite, no llm) ✅
- `cargo build --no-default-features --features libsql` ✅
- `cargo build --no-default-features --features libsql,llm` ✅
- `cargo clippy` (default) ✅ clean
- `cargo clippy --no-default-features --features libsql` ✅ clean

## Deviations from Plan

None significant. The implementation follows the architecture, data model, trait changes, MCP tools, CLI flags, config fields, and migration approach described in `01_planner_plan.md` exactly.
