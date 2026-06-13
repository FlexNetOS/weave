# WL-051 — token-light invariant + standing-token budget gate (ADR-0003)

## Change
- weave-mcp/src/mcp.rs: `pub const MAX_STANDING_TOOLS_BYTES = 8192` (≈2k tokens) +
  test `standing_mcp_surface_is_within_token_budget` — serializes the default `tools()`
  and asserts it stays under budget AND is a handful of tools (not a flat table).
- CLAUDE.md: `token-light` added to the Non-negotiable invariants (peer of dependency-light).
- ARCHITECTURE.md: roadmap bullet marks WL-051 done; budget constant documented.
- CHANGELOG.md [Unreleased]: WL-051 entry.

## Why it matters
"Adding a feature must not add standing tokens." The budget test makes that enforceable
in CI — a revert to eager-flat (~180 KB) or a pile of standing dispatchers trips it.
Eager-flat opt-in (WEAVE_MCP_EAGER=1) is exempt. CLI parity is the zero-standing-cost path.

## Gate
- fmt clean; clippy -D warnings clean (default + libsql); budget test passes both backends.
- NOTE: the local full-suite run shows ONE false failure —
  `peers_json_surfaces_remote_host_peer_alive_remote_additive_keys` asserts `peers --json`
  is secret-free via a blanket substring check for "token"; the WORKTREE PATH
  (`weave-wl051-token-budget`) contains "token", so the cwd field in the output trips it.
  This is a worktree-name artifact, not a code defect — CI's clean checkout path passes.
  (Pre-existing test brittleness; out of WL-051 scope. Worth hardening later.)
