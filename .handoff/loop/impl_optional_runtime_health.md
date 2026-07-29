# Implementer log — optional runtime health / LLM functional hardening

Status: implementation complete.

## What changed

- `reqwest` uses optional `rustls-tls-webpki-roots`; the top-level `llm` feature
  now reaches `weave-mcp`, while the default graph still contains no HTTP/TLS
  client.
- Config honors all five `WEAVE_LLM_*` overlays. Provider requests enforce a
  16,000-Unicode-scalar input cap, a 1–300 second timeout, a 64 KiB raw success
  response limit, disabled redirects, bearer authentication, confidential
  failure messages, and a normalized 16,000-scalar one-paragraph output cap.
- CLI and MCP summarization are real handlers with feature-correct discovery and
  errors. Both use `SUMMARY_THREAD_LIMIT = 200`, independent of CLI display
  limits, and `--refresh` requires the summarize action.
- Both stores persist a summary generation. Additive versioned triggers advance
  it and clear cache on every message mutation. Snapshot reads verify generation
  before and after collection; conditional writes require the same live root and
  generation. Expired, ephemeral, deleted-root, stale-generation, and migrated
  legacy summaries fail closed.
- CLI/MCP revalidate cached rows before output. GC, clear, expiry sweep, and all
  mutation paths invalidate consistently in SQLite and libSQL.
- Environment-sensitive tests share the canonical test lock and RAII restoration
  so parallel tests cannot leak process configuration.
- README, changelog, testing guidance, and security documentation now describe
  provider disclosure, caps, redirect refusal, cache semantics, and the optional
  dependency boundary.

## Files

Seventeen product/test/doc files changed: `CHANGELOG.md`, `Cargo.lock`,
`README.md`, `docs/{SECURITY,TESTING}.md`, both core store implementations,
core config/LLM/memory code, MCP routing, top-level CLI/Cargo features, and the
integration/security test harnesses.

## Implementer gates

- SQLite+LLM full suite: 828 passed.
- libSQL+LLM full suite: 761 passed, 1 intentional live-Turso test ignored.
- Maximal MCP (`llm + obscura`): 53/53 on each backend.
- Strict clippy passed for both LLM backends and both maximal MCP combinations.
- `cargo fmt --all -- --check` and `git diff --check` passed.
