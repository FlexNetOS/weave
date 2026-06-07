# AGENTS.md — weave session handoff

## First command

Run:

```bash
cd /home/drdave/Desktop/meta/weave-mcp-daemon-tools
bash _workspace/verify-on-resume.sh
```

## Mission

Maintain the weave Rust project through the `weave-loop` autonomous backlog system.
The repo is the source of truth. Chat history is not authoritative.

## Hard rules

- Do not edit files without running `verify-on-resume.sh` first.
- Do not write outside the scope of the current WL item.
- Do not run a parallel write session against overlapping paths.
- Do not mark a backlog item complete without tests on BOTH backends (sqlite + libsql).
- Do not stop without updating `_workspace/HANDOFF.md`, `_workspace/loop_state.md`, and `_workspace/backlog.md`.
- Do not treat chat transcript as more authoritative than `HANDOFF.md`, the ledger, or backlog.
- Do not make architecture changes without updating `docs/ROADMAP-v0.X.md`.
- Do not skip the dual-backend gate: `cargo test` on sqlite AND `--no-default-features --features libsql`.

## Required before stopping

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --no-default-features --features libsql
# Update _workspace/HANDOFF.md, loop_state.md, backlog.md
# Commit
```

## Navigation order

1. `_workspace/HANDOFF.md` — authoritative resume signal
2. `_workspace/backlog.md` — current state of all WL items
3. `_workspace/loop_state.md` — cycle counters and budget
4. `TASKS.md` — original M1/M3 source
5. Relevant `docs/ROADMAP-v0.X.md` for architectural context
6. `weave-invariants` skill — before writing or reviewing code
7. `weave-test-discipline` skill — before declaring done

## Skills to consult

| Skill | When to use |
|-------|-------------|
| `weave-invariants` | Before writing or reviewing any src/ code |
| `weave-test-discipline` | Before declaring a change done |
| `weave-orchestrator` | For any feature work, bug fix, or Store change |
| `weave-loop` | When resuming or advancing the backlog |

## State precedence

1. Git HEAD, branch, worktree diff, file contents.
2. Committed `_workspace/HANDOFF.md`.
3. `_workspace/loop_state.md`.
4. `_workspace/backlog.md`.
5. `_workspace/DONE`.
6. Chat transcript — non-authoritative background only.
