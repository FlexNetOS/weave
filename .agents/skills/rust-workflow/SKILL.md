---
name: rust-workflow
description: Implement and verify Rust changes in the weave workspace. Use for Rust code edits, cargo gate planning, backend parity, CLI/MCP/injector behavior, tests, and release-quality Rust workflow.
metadata:
  owner: weave
  type: rust-workflow
---

# rust-workflow

Use this skill for Rust implementation work in the weave workspace.

## Workflow

1. Locate the affected crate: `weave-core`, `weave-inject`, `weave-mcp`, or
   `weave`.
2. Read the existing module and tests before editing. Prefer local patterns over
   new abstractions.
3. Keep behavior in Rust. Sidecar Codex/Claude/agent files may guide work but
   must not become runtime inputs for the compiled binary.
4. Add focused tests for changed behavior. For CLI, MCP, and injector behavior,
   prefer black-box tests under `tests/` when possible.
5. Run the smallest useful gate first, then expand according to impact.

## Verification ladder

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

When touching store traits, backend implementations, MCP protocol behavior, or
shared model types, also run:

```bash
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build --no-default-features --features libsql
cargo test --no-default-features --features libsql
```

## Failure handling

On a failing gate, fix the first actionable failure before broad reruns. Report
the command, the failing file or symbol, and the next focused command.
