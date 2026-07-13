# Plan — optional runtime health / LLM functional hardening

## Goal

Make the optional `llm` summarization surface production-safe and fully usable
without changing the default dependency graph. The CLI and MCP must expose the
same real behavior, both supported stores must preserve identical cache
semantics, and every provider/network boundary must be bounded and confidential.

## Scope

- Propagate the top-level `llm` feature through `weave-mcp` while keeping the
  default build free of HTTP/TLS dependencies.
- Apply all five `WEAVE_LLM_*` configuration overlays and resolve the effective
  provider/model consistently.
- Bound request input, provider time, raw response bytes, and normalized summary
  output; refuse redirects and keep credentials/provider bodies out of errors.
- Replace placeholder CLI/MCP handlers with feature-correct summarization and a
  shared 200-message snapshot.
- Make cached summaries generation-safe across SQLite and libSQL, including
  mutation, expiry, GC, ephemeral-message, root-liveness, and legacy-migration
  cases.
- Add hermetic provider, CLI, MCP, cache-race, migration, feature-surface, and
  security tests; synchronize user and security documentation.

## Invariants

- No HTTP/TLS dependency in the default feature tree.
- Rustls with WebPKI roots only when `llm` is enabled.
- No redirect following, secret-bearing errors, unbounded provider reads, or
  cache writes against a stale/deleted conversation generation.
- Both supported storage backends implement and test the same behavior.
- MCP catalog entries exist if and only if their compile-time feature exists.
- Environment-mutating tests use the canonical lock and panic-safe restoration.

## Required gates

- Full workspace tests for default, SQLite+LLM, and libSQL+LLM.
- Maximal MCP tests for both backends with `llm + obscura`.
- Strict `clippy -D warnings` for both supported LLM backends and maximal MCP
  feature combinations.
- Formatting, diff hygiene, dependency-tree checks, and supply-chain policy.
- Independent verifier and guardian approval before delivery.

## Delivery

Branch `fix/all-feature-health`, based on `origin/develop` at `5a43693`, delivered
as an isolated PR into `develop` before beginning the next health cycle.
