# Goal: Local repowire source parity audit for Weave

Audit Weave's Rust-native repowire port against the local upstream zip:

`/home/drdave/Desktop/meta/meta-yard/repowire-main.zip`

## Why

We previously found that Weave had under-scoped the real repowire browser dashboard. Even though WL-083 is now marked complete, treat that as unproven until a local source audit verifies the claim from code, tests, runtime behavior, and docs.

## Hard rules

- Work in `/home/drdave/Desktop/meta/weave`.
- Use ICM recall before work and ICM store before final response for significant work.
- Preserve Weave invariants: Rust-native, dependency-light, no Node/Next runtime, no shell injection, parameterized SQL, writes bearer-gated and explicit `--write`.
- Do not import repowire code into Weave. Use the zip as reference only.
- For every committed chunk: commit, push, open/update PR, and arm auto-merge immediately.

## Audit steps

1. Verify the zip exists.
2. Unpack it into a gitignored scratch area such as `.handoff/run/repowire-source-audit/repowire/`.
3. Build a repowire code map from the zip:
   - dashboard files/components/hooks/API calls,
   - CLI/MCP/tool surfaces,
   - config/state/schema files,
   - tests/docs defining behavior,
   - daemon/relay/runtime assumptions.
4. Build a current Weave code map from merged `develop`:
   - dashboard routes/forms/events,
   - MCP/CLI tools,
   - config/schema/state,
   - tests/runtime smoke/docs/handoff evidence.
5. Check in an audit artifact, preferably:
   - `.handoff/loop/audit_REPOWIRE_LOCAL_SOURCE_PARITY.md`

## Required audit matrix columns

For every mapped upstream item, record:

- repowire path/symbol/component
- upstream behavior
- Weave path/symbol/route/tool
- proof type: code / test / runtime / doc
- verdict: covered / superset / superseded / gap / unclear
- notes/action

## Dashboard areas that must be covered

- peer roster
- selected peer view
- transcript/thread search/reply/pagination
- mesh feed/event stream/reconnect/gap recovery
- pending questions: choice/tool/free-text
- jobs: list/detail/result/cancel/retry/recreate
- settings/config surface
- spawn/kill/dialog controls and safety posture
- auth/token/cookie/write gating
- JSON/SSE endpoint compatibility

## Non-dashboard surfaces to audit if present

- messaging/ask/answer/notify/broadcast
- scheduling/jobs
- memory/persona/SOUL
- relay/push/federation
- hooks/PreToolUse
- providers/agent runtimes
- scaffolding/agents create
- config/schema/security constraints

## If gaps are found

- Classify each as true gap, superseded behavior, intentional non-goal, or unclear.
- Fix high-priority dashboard gaps in small chunks.
- Add tests/runtime proof.
- Update docs only after proof.
- Commit/push/PR/automerge immediately.

## Suggested verification

```bash
cargo fmt --all --check
cargo clippy -p weave --features surfaces --all-targets -- -D warnings
cargo test -p weave-mcp --features surfaces
cargo test -p weave --features surfaces --test integration surfaces_dashboard -- --nocapture
```

Add targeted tests for any discovered gaps.

## Definition of done

The local repowire zip has been fully mapped against current Weave, the audit artifact is checked in, gaps are fixed or explicitly classified, verification passes, changes are committed/pushed with PR and auto-merge armed, and the parity claim is backed by local source evidence rather than memory or old summaries.
