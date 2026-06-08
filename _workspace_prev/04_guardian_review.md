# WL-026 Guardian Review

## Invariant audit

| Invariant | Status | Notes |
|---|---|---|
| No shell | ✓ | No new command spawning; existing argv-only injection paths unchanged |
| Parameterized SQL | ✓ | All idempotency_key / trace_id values bound as params; never interpolated |
| Input caps | ✓ | `MAX_IDEMPOTENCY_KEY_LEN` (128) and `MAX_TRACE_ID_LEN` (128) enforced in model validators and store `send` |
| MCP stdout discipline | ✓ | Tool responses unchanged; trace_id only in result body, never on stdout outside JSON-RPC |
| Paste-safe injection | ✓ | No changes to inject path |

## Drift guard
- No new non-Rust files
- No build.rs changes
- No new default dependencies
- `getrandom` already a `weave-core` dependency (used for birth certs, nonces)

## Docs sync
- `CHANGELOG.md` [Unreleased] should note: "Added per-message idempotency keys and distributed trace IDs (WL-026)"
- README / ARCHITECTURE do not require updates for this internal feature

## Verdict
**APPROVE**

Minor follow-up: update CHANGELOG.md [Unreleased] before merge.
