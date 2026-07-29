# Verifier report — trusted runtime, identity, liveness, and dispatch health

Verdict: **GREEN — no blocking findings.**

## Independent verification

- The exact final tree passed the 839-test default workspace. Maximal SQLite
  passed 506 tests; maximal libSQL passed 490 with one intentional live-Turso
  test ignored. Core libSQL passed 272 and maximal MCP libSQL passed 60.
- Strict maximal workspace clippy passed on SQLite and libSQL. Formatting, diff
  hygiene, documentation freshness, dependency inspection, and the repository
  supply-chain audit are green. `cargo-deny` is not installed locally, so the
  audit's installed-tool advisory gate remains delegated to CI.
- SQLite and libSQL queued-only claims were reviewed for atomicity and preserve
  the separate manual re-claim/fencing contract. Invalid deterministic input
  remains queued; post-claim runner failures become bounded terminal records.
- Resolver parity, direct-parent trust, configured-root rejection, nonzero mux
  verdicts, bounded probe capture, exact session ownership, explicit PID rules,
  read-only connect wording, and the feature-gated command ledger were replayed.
- Boundary tests cover oversized and invalid hook input, including a multibyte
  UTF-8 scalar split exactly at the raw-byte limit. Runner and probe regressions
  cover detached descendants retaining inherited output pipes.

## Findings closed during review

- Runner and mux-probe collection initially waited for inherited pipe EOF after
  the leader exited. Both paths now stop their bounded drain promptly and reap
  the owned leader; detached-descendant regressions pass.
- Oversized pre-tool input was initially classified after JSON decoding. Raw
  bytes are now bounded first, so size policy remains correct for malformed UTF-8
  and a scalar split at the limit.
- Edge-whitespace session keys could initially normalize onto an existing row.
  Keys are now exact tokens and such inputs fail closed on both stores and the
  black-box hook path.
- The maximal `obscura` lane exposed a missing `web` entry in the expected/TUI
  command catalog. The ledger and tests now include the feature-gated surface.

## Test-isolation note

During manual lifecycle-hook review, two inert sentinel sends omitted the
temporary `WEAVE_DB` assignment and reached the live local store. They were not
read, marked, deleted, or otherwise altered after discovery, and the reviewer ran
no further commands against that state. This was a review-fixture isolation
mistake, not a product-code finding.
