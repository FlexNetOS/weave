# Implementer log — trusted runtime, identity, liveness, and dispatch health

Status: implementation complete.

## What changed

- Trusted program resolution is shared by injection, Obscura, and job dispatch.
  Bare names must be one normal path component; absolute programs must be
  executable and have a canonical direct parent in an approved directory.
  Empty or relative configured trust roots are ignored.
- Zellij, WezTerm, and Kitty inventory failures now produce an honest structured
  transport verdict while the legacy advisory boolean remains fail-open. Probe
  output is drained concurrently into fixed-size buffers, and a completed probe
  leader cannot be held open indefinitely by a detached descendant.
- CLI and MCP `connect` are explicitly read-only reachability checks. Their
  output no longer implies that a message was sent or queued.
- One-shot registration surfaces only bind a process when a positive
  `WEAVE_CLIENT_PID` is supplied. Hook ownership uses an exact, bounded,
  control-free launcher session key; edge whitespace is rejected, conflicting
  nonempty keys cannot alias through a PID, and an unowned basename fallback is
  peek-only.
- Hook input is bounded as raw bytes before UTF-8 or JSON parsing. Oversized
  pre-tool input receives the conservative deny response, while malformed
  non-oversized input remains non-mutating.
- Dispatch performs all deterministic validation before an atomic queued-only
  claim on SQLite or libSQL. Once claimed, normal launch, capture, timeout, and
  wait failures durably terminalize the attempt. Runner processes are grouped,
  terminated, reaped, concurrently drained, and stored within fixed JSON caps.
  NUL-bearing job text is rejected at both the store seam and legacy-row replay.
- The maximal command ledger includes the feature-gated `web` surface. README,
  changelog, operations, testing, and security documentation now match the
  implemented reachability, identity, hook, resolver, and runner semantics.

## Files

Fourteen product, test, and documentation paths changed: five public docs, both
store implementations, injection and MCP routing, the top-level CLI/runner, the
integration suite, and a new external trusted-resolution regression test. No
`Cargo.toml` or `Cargo.lock` change was required.

## Implementer gates

- Exact-tree default workspace: 839 passed, 0 failed.
- SQLite maximal top-level (`sign,llm,surfaces,obscura`): 506 passed.
- libSQL maximal top-level: 490 passed, 1 intentional live-Turso test ignored.
- libSQL core: 272 passed; maximal libSQL MCP: 60 passed.
- Focused SQLite/libSQL atomic-claim, exact-session-key, hook-byte-boundary,
  trusted-resolution, nonzero/large/detached-probe, and detached-runner replays
  all passed.
- Strict maximal workspace clippy passed for both supported storage backends.
- `cargo fmt --all -- --check`, `git diff --check`, documentation freshness,
  dependency-tree inspection, and the structural supply-chain audit passed.
