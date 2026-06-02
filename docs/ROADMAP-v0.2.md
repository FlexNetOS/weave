# weave v0.2 roadmap

Synthesized from a multi-agent design panel (5 designs × 4 judge criteria) and a
competitive research sweep. The two winning designs are **complementary**: a
**workspace split** is the *substrate* (clean seams, no drift), and an **optional
presence daemon** is the *first feature* built on it. The default `cargo build`
stays a single statically-linked binary with **no daemon and no tokio** at every
phase — the workspace is an internal refactor, not a packaging change.

## Guiding constraints
- **Additive, no regressions.** The no-daemon local path is the tested fallback, never second-class.
- **Backends stay mutually-exclusive** (`sqlite` default XOR `libsql`); the trait stays **sync**; tokio stays confined to `LibsqlStore`.
- Every phase keeps `cargo test` green for **both** feature columns.

## v0.2.0 — Foundation (no new runtime behavior)
- **Phase 0 — debt paydown (DONE in the current tree).** Both backends now implement the full `Store` trait (reply/thread/receipts/touch_peer/in_reply_to + migration); `--no-default-features --features libsql` builds, clippy-`-D` clean; 64 tests green on the default backend. The historical "both backends green" gap is closed.
- **Phase 1 — carve the workspace (mechanical, behavior-identical).** Move `model`+`store`+`store_libsql`+`config` → `weave-core`; `inject` → `weave-inject`; `mcp` → `weave-mcp`; keep `weave` (bin) for CLI+setup+hooks. Re-export to minimize churn. Make `weave-mcp::serve` generic over an `Injector` trait so MCP is testable **without a real mux or DB** (today's biggest test gap). Exit gate: identical `cargo build` output, one binary, all tests + a new `assert_store_conformance(&dyn Store)` suite green per backend.
- **Phase 2 — presence seam, still no daemon.** Add a guarded `presence` table migration (mirrors the `in_reply_to` pattern). Add a two-tier `presence()` resolver: a fresh daemon heartbeat (≤30s) wins; absent/stale falls back transparently to the v0.1 `is_online(last_seen)` 900s heuristic. Add a cheap `host` column now so cross-machine *presence* is near-free later. Exit gate: with no daemon writing rows, `weave peers` output is byte-identical to v0.1.

## v0.2.x — the daemon (behind a `presence` feature, OFF by default)
- `weave-proto` (serde-only wire types) + `weave-daemon` (`weaved`): newline-delimited JSON over a `0600` UDS at `$XDG_RUNTIME_DIR/weave/weaved.sock`; registry + reconcilers + lifecycle eviction (tmux/zellij pane-exit hooks). Four independent optionality gates so it always degrades to v0.1.

## v0.3 — richer delivery & reach (tail work)
- Nullable `deliver_at`/`kind` columns + `weave_schedule`/`weave_ack` MCP tools (pure additions to `tools()`).
- Cross-machine **presence** via libSQL embedded replicas (no API change above the backend); cross-machine *injection* stays explicitly out of scope.
- More terminal backends: iTerm2; refine kitty/wezterm/screen liveness.

## Positioning (from the research sweep)
weave is the **single-binary agent mesh** that replaces brittle, polling-based,
`tmux send-keys` multi-agent setups with reliable cross-agent handoff. The
highest-leverage differentiators to pursue:
1. **No-poll stop-boundary wake** — a blocking `Stop`/`SubagentStop` hook that
   queries weave's local store and returns `additionalContext` so a peer's message
   *drives the next turn* with no in-agent poll loop. (This is the design that
   structurally beats keystroke injection; weave already has the hook-drain seam.)
2. **Standard hook-contract adapter** — speak the stdin-JSON → stdout-JSON →
   exit-`0`/`2` contract that Claude Code, Codex, and Gemini CLI have converged on,
   so one adapter yields three integrations.
3. **Read-side SSE stream** alongside the write-side hooks, to cover OpenCode-style
   observers/dashboards.

> Note: the competitive briefs were partly rate-limited; the market-comparison axis
> (orchestrators, MCP ecosystem, Turso) is under-sourced and should be re-run before
> committing roadmap weight to it.
