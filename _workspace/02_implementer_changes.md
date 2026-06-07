# Implementer changes — WL-010

## What changed
- **ARCHITECTURE.md §8** — appended a retirement decision callout after the P5 parity
  bullet. States that mcp-broker and repowire are retired on this box now that weave
  has achieved parity (P1–P5) and subsumes broker semantics.
- **TASKS.md** — flipped the retirement line to `- [x]`.
- **backlog.md** — flipped WL-010 to `- [x]` with an evidence note.

## No Rust code changes
Zero source edits; no store, model, inject, MCP, or CLI changes.

## Build confirmation
- `cargo build` — expected green (no code changed)
- `cargo build --no-default-features --features libsql` — expected green
