# Guardian review — trusted runtime, identity, liveness, and dispatch health

Verdict: **APPROVE — cleared for delivery.**

## Invariant review

- Trusted resolution is shared and exact: bare programs are one normal
  component, absolute programs require an executable whose canonical direct
  parent is approved, and invalid configured roots contribute no trust.
- Structured mux reachability reports nonzero probes honestly. Probe and runner
  output paths remain finite in time and space even when detached descendants
  retain inherited descriptors.
- One-shot presence only binds explicit positive client PIDs. Hook ownership is
  exact and bounded, conflicting launcher tokens cannot alias through a PID, and
  fallback identity cannot consume an inbox.
- Hook input is byte-bounded before decoding. The exact MAX+1 split-UTF-8 case is
  classified as oversized and leaves inbox state unread in an isolated fixture.
- Dispatch validates before an atomic SQLite/libSQL queued-only claim. Owned
  processes are terminated and reaped, output and durable JSON are bounded, and
  all ordinary post-claim failures reach a terminal result.
- CLI/MCP connect wording is read-only and the maximal command catalog includes
  the feature-gated `web` surface.
- README, changelog, security, testing, and operations documentation match the
  implementation, including the distinction between operator inbox reads and
  hook basename fallback.

## Independent replay

- `weave-inject`: 64 unit tests plus the external trusted-resolution test.
- SQLite and libSQL dispatch regressions: 9/9 on each backend.
- SQLite and libSQL WL-084 identity regressions: 14/14 on each backend.
- Exact MAX+1 split-UTF-8 hook boundary: 1/1 on each backend.
- Connect regressions: 3/3; queued-only claim regressions: 2/2 per backend.
- Maximal SQLite/libSQL help and TUI command-ledger tests passed with `web`.
- Formatting and diff hygiene passed. The guardian made no edits and used only
  isolated temporary databases for runtime fixtures.

No Cycle B correctness, resource-bound, identity, backend-parity,
documentation, or release blocker remains.
