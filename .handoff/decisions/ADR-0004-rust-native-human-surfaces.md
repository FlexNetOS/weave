# ADR-0004 — Rust-native human surfaces (dashboard + Telegram + Slack) behind `--features surfaces`

- **Status:** accepted — 2026-06-13 (owner-confirmed in scope)
- **Plane:** agent-mesh
- **Owner:** drdave
- **Scope:** weave human surfaces — a live read-only web dashboard and Telegram/Slack
  bridges — all behind a single new `--features surfaces` Cargo feature (default OFF).
  No change to weave's core invariants or default binary.
- **Supersedes/relates:** ADR-0003 (token-light multi-surface — preserved: the surfaces
  are CLI subcommands, NOT standing MCP tools); WL-022 (HTTP bearer auth, reused);
  WL-033 (`llm` reqwest blocking client, shared); REPOWIRE-PARITY §6 (the last parity gap).

## Context

repowire shipped three **human surfaces**: a Next.js web dashboard, a Telegram bot, and a
Slack bot. weave's only human surface today is the `weave sessions --watch` terminal TUI.
That is the last genuine repowire-parity gap (REPOWIRE-PARITY §6).

Constraint: closing it must **not** break weave's non-negotiables — one dependency-light
static binary, no Python, no Node/Next.js runtime, no-shell argv-only spawning, MCP stdout
discipline, and heavyweight/tokio-tree deps behind feature flags only (as `libsql` is).
The default `cargo build` must gain **zero** new compiled dependencies.

## Decision (LOCKED)

1. **Web stack = the existing hand-rolled `std::net` HTTP transport.** The dashboard is
   server-rendered HTML (via `format!`/string building, NO template engine) plus
   Server-Sent Events (`text/event-stream`) served over the SAME `std::net::TcpListener`
   in `weave-mcp/src/http.rs` that already serves the MCP JSON-RPC POST surface. The new
   `GET /` (HTML page) and `GET /events` (SSE) routes are added to `handle_connection`; the
   POST/JSON-RPC path stays byte-identical. **NO axum/tokio/hyper/warp/actix.**
2. **Bot HTTP client = `reqwest` (blocking + rustls), already an optional dep.** reqwest is
   already declared optional in `weave-core/Cargo.toml` under `llm`. The `surfaces` feature
   ALSO enables `dep:reqwest`, so Cargo unions the feature and the two surfaces SHARE one
   reqwest copy (confirmed: `cargo tree -e features` shows exactly one reqwest with
   `surfaces`, and zero in the default build). Bots are **poll-only v1** (Telegram
   `getUpdates` long-poll; Slack poll) — NO inbound webhook server to expose.
3. **Everything behind `--features surfaces`, default OFF** — the default binary gains zero
   compiled deps. `weave-core` `surfaces = ["dep:reqwest"]`; `weave-mcp`
   `surfaces = ["weave-core/surfaces"]`; `weave` `surfaces = ["weave-core/surfaces",
   "weave-mcp/surfaces"]` — mirroring the `sign`/`libsql` propagation chain.
4. **Surfaces are CLI subcommands, NOT standing MCP tools** (`weave dashboard` /
   `weave telegram` / `weave slack`) — ADR-0003 token-light preserved; the MCP tool table
   is not bloated.
5. **No Next.js, no Python, dependency-light + token-light preserved.** SSE uses a
   thread-per-connection accept model (`std::thread`, NO async runtime) so a long-lived SSE
   stream cannot starve the MCP port.

## Alternatives considered (rejected)

- **axum / tokio / hyper / warp / actix** — rejected: pulls a large async-runtime dependency
  tree into the binary, violating the dependency-light + one-small-static-binary invariant.
  The existing hand-rolled `std::net` transport already serves HTTP with zero extra deps.
- **Next.js / any JS dashboard** — rejected: re-adds a Node build step and a non-Rust runtime;
  breaks the no-Python/no-Node, single-Rust-binary posture.
- **A second HTTP client crate for the bots** — rejected: reqwest is already present under
  `llm`; reusing it via a unioned feature keeps the dependency tree to one client copy.
- **Standing MCP tools for the surfaces** — rejected: adds a permanent tool-table token cost
  (ADR-0003 token-light). CLI subcommands cost nothing at the MCP surface.
- **Inbound webhook server for the bots (v1)** — rejected for v1: an exposed inbound webhook
  is an extra attack surface; poll-only (long-poll + bounded interval + reqwest timeout)
  delivers the same relay with no listening socket. Webhook mode is deferred.

## Consequences

- The last repowire-parity gap closes **Rust-native** with **zero added weight** to the
  default binary (reqwest is unified with `llm`; default `cargo tree` shows no reqwest).
- A new **exposed read surface** (the dashboard) and **new secrets** (Telegram/Slack bot
  tokens) enter the threat model: the dashboard is read-only (GET only, never mutates),
  binds `127.0.0.1` by default, is bearer-gated (WL-022), and HTML-escapes EVERY
  Store-derived string (the central XSS defense); bot tokens are config/env values,
  Debug-redacted, never logged, never placed in a logged URL/argv. envctl can inject the
  token env vars into the process.
- CI gains a `surfaces` build/test job on BOTH backends (`--features surfaces` and
  `--no-default-features --features "libsql surfaces"`).

## Research / Cross-references

- `weave-mcp/src/http.rs::serve_http` / `handle_connection` — the std HTTP transport + WL-022
  bearer auth the dashboard rides on (same listener, GET routes added).
- `weave-mcp/src/dashboard.rs` — the new pure render/escape/SSE/route module (socket-free,
  testable; XSS-escapes all Store strings).
- `weave-core/src/llm.rs` — the `reqwest::blocking` + rustls client pattern the bots mirror
  (config-field-first, env-fallback secret precedence; 30s timeout).
- The four Cargo `[features]` tables (`surfaces` propagation) and ADR-0003 (token-light:
  CLI subcommands, not MCP tools).
- repowire's Next.js dashboard + Telegram/Slack bots — the parity target closed here
  Rust-native rather than by re-adding a Node/Python runtime.
