# WL-052 — multi-surface parity (foundation: audit + decomposition)

## Scope decision
"Full multi-surface parity" (every capability on CLI+MCP+dashboard+bots) is a multi-step
effort; the human-surface WRITE paths (dashboard forms, bot command grammar) are
security-sensitive (hand-rolled HTTP POST routing/auth/CSRF; chat parser) and not safe to
rush in one session. This card lands the MEASURABLE FOUNDATION and decomposes the rest.

## Change (docs only — no Rust, no dep, no schema)
- NEW `docs/MULTI-SURFACE-PARITY.md`: capability × surface matrix with per-cell verdicts.
  Result: CLI + MCP = FULL parity (agent surfaces); dashboard (read-only) + bots (relay) = WL-048 v1 baseline.
- ARCHITECTURE.md: reference the new matrix next to REPOWIRE-PARITY.md.
- CHANGELOG.md [Unreleased]: WL-052 entry.
- backlog.md: WL-050/051/053 marked done; WL-052 foundation done; added WL-052a (dashboard write)
  + WL-052b (bot commands) with the design law "route to ONE handler, never re-implement per surface".

## Verification
Docs-only; no Rust touched (git diff is .md only) → default cargo tree unchanged, gates trivially green.
