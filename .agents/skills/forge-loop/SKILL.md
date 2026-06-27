---
name: forge-loop
description: Rust-native Codex forge loop for Weave task execution. Use when asked to run /forge-loop, continue autonomous task execution, or coordinate Codex subagents for Weave implementation work.
---

# forge-loop

Run one cohesive Weave task cycle through the Rust CLI front door, not a prompt-only workflow.

## Entry point

Prefer:

```bash
weave harness forge-loop --task "<objective>"
```

This dry-runs the execution plan. Add `--execute` only when the operator requested execution.

## Cycle contract

1. Recover durable state first: git status, `.handoff/`, ICM, and existing Codex context.
2. Pick exactly one cohesive task; do not expand scope mid-cycle.
3. Use Codex subagents for read-heavy exploration/review; keep write ownership single-threaded.
4. Preserve Weave invariants: Rust-native, argv-only process spawning, no shell strings, no new default-heavy deps.
5. Verify in fresh shells: fmt, clippy, tests, and feature/backend gates appropriate to the touched code.
6. Deliver immediately when green: commit, push, open PR to `develop`, and arm auto-merge.
7. Halt with a committed `NEEDS-HUMAN` sentinel for auth/sudo/hardware/branch-protection walls.

## Codex surface policy

- This skill is the durable workflow source of truth.
- `/forge-loop` is only a convenience shim installed by `weave codex-tools install`.
- Project `.codex/config.toml` remains an inert repo baseline; user-level Codex config and secrets stay outside the repo.
