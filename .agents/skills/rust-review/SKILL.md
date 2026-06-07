---
name: rust-review
description: Review Rust changes in weave for correctness, security, behavior regressions, missing tests, backend parity, and drift away from a dependency-light Rust binary.
metadata:
  owner: weave
  type: rust-review
---

# rust-review

Use this skill for review requests involving Rust code, cargo gates, or weave
behavior.

## Review posture

Lead with findings. Prioritize:

- Correctness bugs.
- Security regressions.
- CLI, MCP, Store, or injector behavior changes without tests.
- SQLite/libSQL parity gaps.
- New dependencies or non-Rust runtime inputs.
- Tests that assert implementation details instead of behavior.

## Evidence

Cite file paths and line numbers. If no findings are found, say so and list the
remaining test gaps or commands not run.

## Verification checks

Map changed files to cargo gates. Store/backend edits need default and libSQL
coverage. CLI/MCP/injector edits need black-box coverage where practical.
