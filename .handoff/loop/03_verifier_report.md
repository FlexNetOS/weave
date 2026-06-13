# WL-050 verifier report — GREEN (both backends)

## Gate
- `cargo fmt --all --check` — clean.
- `cargo clippy --all-targets -- -D warnings` (sqlite) — No issues.
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` — No issues.
- `cargo clippy --features sign --all-targets -- -D warnings` — No issues.
- `cargo clippy -p weave-mcp --features obscura --all-targets -- -D warnings` — No issues.
- `cargo test --all-targets` (sqlite) — 577 passed (was 566; +11).
- `cargo test --no-default-features --features libsql` — 537 passed, 1 ignored (was 526; +11).
- `cargo test -p weave-mcp --features obscura weave_web_is_registered` — ok (repointed to tool_catalog).

## Test layers added (11)
- Unit (mcp.rs #[cfg(test)]): progressive_default_surface_is_just_the_meta_tool; eager_mode_restores_the_full_flat_table;
  meta_search_finds_ops_by_keyword; meta_list_enumerates_every_op; meta_describe_returns_schema_or_errors;
  meta_call_matches_direct_dispatch; meta_call_guards_recursion_and_unknown; meta_rejects_bad_mode;
  meta_call_preserves_safe_http_gate; every_catalog_op_is_dispatchable (catalog↔dispatch drift guard).
- Integration (real binary): mcp_progressive_disclosure_default_surface_and_meta_roundtrip
  (tools/list == [weave], then search→describe→call(send)→call(inbox) roundtrip).

## Cross-boundary checks
- tools/call with a flat name (e.g. weave_scan) still dispatches — only the standing tools/list advertisement changed.
- Safe-HTTP gate parity: dangerous inner op via meta=call is rejected identically to the flat path.
- every_catalog_op_is_dispatchable proves no catalog entry lacks a dispatch arm.
