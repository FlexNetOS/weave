# weave — Multi-Surface Parity Matrix (WL-052 / ADR-0003)

**Claim under audit (WL-052):** every weave capability is reachable on every surface —
**CLI**, **MCP**, **HTTP/SSE dashboard**, **Telegram/Slack bots** — each feature-flagged for
build-leanness, none feature-reduced. "More than repowire, **on every surface**, not less."

This document makes that claim **measurable**, not asserted: it maps every capability domain
onto each surface with a concrete verdict, so a gap is *tracked*, not silently shipped. It is
the multi-surface analogue of `REPOWIRE-PARITY.md`, and the ledger ADR-0003 point 1 (full
multi-surface parity) is checked against.

## Surfaces

| Surface | Build | Standing token cost | Audience | Status |
|---|---|---|---|---|
| **CLI** (`weave <subcmd>`) | default | **zero** (pay per invocation) | agents (token-bound) + humans | the reference surface — **full** |
| **MCP** (`weave mcp`) | default | bounded (`weave` meta-tool, WL-050/051) | agents that speak MCP | **full** — all ops via meta-tool `call` |
| **Dashboard** (`weave dashboard`, HTTP/SSE) | `--features surfaces` | n/a (out-of-band) | humans (browser) | **read-only v1** (WL-048) |
| **Bots** (`weave telegram` / `weave slack`) | `--features surfaces` | n/a (out-of-band) | humans (chat) | **relay v1** (WL-048) |

Legend: ✅ reachable · ◐ partial · ❌ not yet · — n/a for this surface.

## Capability × surface matrix

| Capability domain | CLI | MCP | Dashboard | Bots | Notes |
|---|:--:|:--:|:--:|:--:|---|
| **Messaging** (send / notify / reply / broadcast) | ✅ | ✅ | ❌ | ◐ | Bots: inbound human→agent relay + outbound notify; no broadcast/reply form. Dashboard: read-only (no send form → **WL-052a**). |
| **Read views** (inbox / history / search / thread / receipts / delivery / outbox) | ✅ | ✅ | ◐ | ❌ | Dashboard renders presence + recent activity (SSE); per-message views are **WL-052a**. Bots: no readback command → **WL-052b**. |
| **Asks** (ask / answer / ack / asks / ask-get / ask-many / broadcast-ask) | ✅ | ✅ | ❌ | ❌ | Structured ask/answer on human surfaces → **WL-052a/b**. |
| **Peers & presence** (peers / sessions / scan / register / attach / connect / doctor) | ✅ | ✅ | ◐ | ❌ | Dashboard shows the presence grid (read-only); that is the v1 baseline. |
| **Spawn / kill** (WL-047) | ✅ | ✅ | ❌ | ❌ | Mutating mux ops are intentionally **agent-surface only** (CLI/MCP); not exposed to human chat/web by design (blast radius). |
| **Jobs** (P3 board: create/list/show/status/claim/update/result/cancel) | ✅ | ✅ | ❌ | ❌ | Read views are a natural dashboard add → **WL-052a**. |
| **Leases** (reserve / release / list / sweep) | ✅ | ✅ | ❌ | ❌ | — |
| **Orchestrator** (claim / status, P4) | ✅ | ✅ | ❌ | ❌ | — |
| **Schedules** (schedule / schedules / cancel / tick) | ✅ | ✅ | ❌ | ❌ | — |
| **Memory** (write / read / list / search / delete) | ✅ | ✅ | ❌ | ❌ | — |
| **Permissions** (list / resolve / status / ask-permission) | ✅ | ✅ | ❌ | ❌ | — |
| **Governed web** (`weave web` / `weave_web`, obscura) | ✅ | ✅ | — | — | `--features obscura`; deny-by-default. Agent-surface by design. |
| **Daemon** (start / stop / status) | ✅ | ✅ | — | — | Process-lifecycle; agent/operator surface. |
| **Summarize** (text / thread, LLM) | ✅ | ✅ | ❌ | ❌ | `--features llm`. |
| **Admin** (setup / uninstall / clear / gc / config) | ✅ | ◐ | — | — | Some admin ops are CLI-only by design (host wiring, retention). |

### The headline result

- **CLI and MCP are at full parity** — every capability domain is ✅ on both. These are the
  **agent-facing** surfaces and the ones that matter for the orchestration mesh; the mission's
  "more than repowire" holds on both. (CLI ≈ 40 subcommands; MCP = the full `tool_catalog()`
  reachable via the `weave` meta-tool. The two stay in lock-step.)
- **Dashboard and bots are deliberately the v1 baseline** (WL-048): the dashboard is a read-only
  presence/activity view; the bots relay text (inbound human→agent + outbound notify). They are
  **not feature-reduced in principle** — the remaining work is *write/parity completeness*, scoped
  below — and the build stays lean because both are behind `--features surfaces` (default OFF).

## Remaining work — tracked, not silent (the WL-052 decomposition)

Full human-surface write-parity is a multi-step effort; it is decomposed into concrete cards so a
gap is never mistaken for "covered":

- **WL-052a — Dashboard write (v1 DONE).** `weave dashboard --write` exposes a bearer-gated
  `POST /api` JSON-RPC action route that dispatches through the **same** `dispatch_request` →
  `call_tool` handler as MCP/CLI — no parallel path; every invariant inherited. Read-only is the
  default (POST → 403). Remaining polish: an in-page HTML send form + per-message/job read views
  (the API is the substrate; the form is a leaf, per the agent-first stance).
- **WL-052b — Bot command grammar.** Structured commands on Telegram/Slack (`/inbox`, `/ask`,
  `/peers`, …) mapping to the same handlers, with the existing identity-sanitization + secret-free
  logging guarantees. Currently the bots relay free text only.
- **Design law for both:** a human surface must call the **same** capability handler as CLI/MCP —
  parity is achieved by *routing to one implementation*, not by re-implementing per surface. That
  is what keeps this matrix honest and the behavior identical everywhere.

## Why this is the right v1 boundary

Agents drive weave through CLI/MCP (full parity, token-light). Humans observe through the
dashboard and nudge through chat. Shipping the **read/relay** human surfaces first (WL-048) and
**measuring** the write gap here (WL-052) — rather than rushing write paths into a hand-rolled
HTTP server and a chat parser — keeps the security invariants (no-shell, parameterized SQL,
input caps, destructive-op gating) intact while making the remaining parity work explicit and
routable to a single implementation.
