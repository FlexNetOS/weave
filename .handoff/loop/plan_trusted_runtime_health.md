# Plan — trusted runtime, identity, liveness, and dispatch health

## Goal

Close the remaining trust-resolution, presence, reachability, session-ownership,
and job-runner gaps without weakening Weave's no-shell, dependency-light, dual-
backend contract. Tests are written RED first; production changes follow only
after each failure reproduces.

## Workstreams

### Trusted program resolution

- A bare trusted program is exactly one `Component::Normal`; reject absolute,
  nested, dot, and traversal input at `resolve_trusted`.
- Ignore empty or relative `WEAVE_MUX_DIR`/`HOME` trust roots.
- Export the existing strict `resolve_trusted_program`: accept a bare program or
  an executable absolute program whose canonical parent exactly equals a trusted
  directory.
- Route spawn/hooks, Obscura, and job dispatch through that single rule.

### Reachability and probe resources

- A nonzero zellij/wezterm/kitty listing is transport-unavailable, never live.
- Preserve `target_alive` as an advisory fail-open bool while keeping structured
  `Capability` honest.
- Drain probe output while retaining a fixed prefix so large listings cannot fill
  a pipe or grow memory without a bound.
- Make CLI/MCP `connect` explicit that it is a read-only probe: no message is sent
  or currently queued; wording describes what a future send would do.

### Presence and session ownership

- One-shot CLI register/attach/scan/watch accept only an explicit valid
  `WEAVE_CLIENT_PID`; absent override means PID `NULL` and TTL liveness.
- Hooks retain the bounded ancestor inference; the long-running MCP server keeps
  its own live PID.
- Different nonempty launcher session keys cannot collapse through equal PID.
- Bound hook input before JSON parsing. Validate one exact, control-free,
  length-bounded session key before lookup/comparison/storage; reject edge
  whitespace and never trim or truncate an ownership key.
- Invalid nonempty keys fail closed without registration or inbox consumption.
  Missing keys may resolve through one unique local PID+host row; otherwise inbox
  handling is peek-only.

### Job dispatch lifecycle

- Complete deterministic preflight before claim: trusted executable, argv count
  and element bounds, agent bound, timeout range/checked deadline, job/env safety,
  and bounded serialized lease snapshot.
- Add an atomic queued-only dispatch claim on SQLite and libSQL. Preserve the
  existing manual re-claim/fencing behavior of `claim_job`.
- Once claimed, every runner spawn/poll/wait/capture failure becomes a bounded
  terminal `failed` outcome; no normal error path strands `running`.
- Own, terminate, and reap the runner lifecycle; drain stdout/stderr concurrently,
  discard beyond fixed capture caps, and guarantee result/error JSON remains at
  or below `MAX_JOB_JSON` even after JSON escaping.
- Reject NUL-bearing job text at the store seam and revalidate legacy rows before
  dispatch.

## RED gates

- Unit: trusted path matrix, nonzero/large probe behavior, explicit PID parser,
  session-key/conflict matrix, bounded JSON/output helpers, queued-only claims on
  both stores.
- Black box: traversal/absolute Obscura and runner refusal; nonzero mux connect;
  PID-null TTL behavior; same-PID/different-session aliases; invalid/oversized
  hook identity; truthful connect with an unchanged inbox; invalid preflight leaves
  jobs queued; post-claim launch failure terminalizes; noisy/escape-heavy/timeout
  runners finish with bounded durable results.
- Run every store-state test on default SQLite and `--no-default-features
  --features libsql`; run Obscura tests on both maximal backend lanes.

## Final gates

- Full default, SQLite maximal, libSQL, and libSQL maximal test matrices.
- Strict clippy for both supported backends and maximal feature graphs.
- Formatting, diff hygiene, dependency tree, supply-chain audit, independent
  verifier, and guardian review.

## Delivery

Branch `fix/trusted-runtime-health`, based on merged `origin/develop` at
`e6b2517` (Cycle A PR #179). Deliver as an isolated PR before bridge work.
