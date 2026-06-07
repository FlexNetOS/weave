# Plan — WL-010: Decide retirement of mcp-broker / repowire

## Goal
Record the retirement decision: weave has achieved functional parity with repowire
(P1–P5) and subsumes mcp-broker's core semantics. Both predecessor tools are
considered retired on this box.

## Changes
1. **ARCHITECTURE.md §8** — append a retirement decision callout at the end of the
   comparison section (before the §9 separator).
2. **TASKS.md** — flip the retirement line to `- [x]`.
3. **backlog.md** — flip WL-010 to `- [x]` with a one-line evidence note.

## No-code nature
- Zero Rust source changes.
- No test additions (the gate is a drift-check only).
- No invariant scope (docs only).

## Verify
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`
- `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings`
- `cargo test --no-default-features --features libsql`

## Risks
None. This is a documentation-only decision record.
