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
| **CLI** (`weave <subcmd>`) | default | **zero** (pay per invocation) | agents (token-bound) + humans | the reference surface — every command carries an MCP decision |
| **MCP** (`weave mcp`) | default | bounded (`weave` meta-tool, WL-050/051) | agents that speak MCP | full catalog where appropriate; intentional CLI-only decisions are ledgered |
| **Dashboard** (`weave dashboard`, HTTP/SSE) | `--features surfaces` | n/a (out-of-band) | humans (browser) | **read-only v1** (WL-048) |
| **Bots** (`weave telegram` / `weave slack`) | `--features surfaces` | n/a (out-of-band) | humans (chat) | relay + command grammar (WL-048/WL-073) |

Legend: ✅ reachable · ◐ partial · ❌ not yet · — n/a for this surface.

## Capability × surface matrix

| Capability domain | CLI | MCP | Dashboard | Bots | Notes |
|---|:--:|:--:|:--:|:--:|---|
| **Messaging** (send / notify / reply / broadcast) | ✅ | ✅ | ❌ | ◐ | Bots: inbound human→agent relay + outbound notify, plus gated `/send` and `/reply` via shared dispatcher. Broadcast/notify forms remain agent-surface only. Dashboard: read-only (no send form polish → **WL-052a**). |
| **Read views** (inbox / history / search / thread / receipts / delivery / outbox) | ✅ | ✅ | ◐ | ◐ | Dashboard renders presence + recent activity (SSE); per-message views are **WL-052a**. Bots answer `/inbox`; richer readback remains future polish. |
| **Asks** (ask / answer / ack / asks / ask-get / ask-many / broadcast-ask) | ✅ | ✅ | ❌ | ◐ | Bots support gated `/ask` + `/answer` through the shared dispatcher; ack/ask-many/broadcast-ask remain agent-surface commands. |
| **Peers & presence** (peers / sessions / scan / register / attach / connect / doctor) | ✅ | ✅ | ◐ | ◐ | Dashboard shows the presence grid; bots answer `/peers` and `/sessions`. |
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
| **Admin** (setup / uninstall / clear / gc / config) | ✅ | ◐ | — | — | Some admin ops are CLI-only by design (host wiring, retention). **Multi-provider host wiring (WL-042, casr parity):** `weave setup --provider <claude\|codex\|gemini\|aider>` wires weave into each host's own config file with the same never-clobber-foreign, idempotent, read-back-verified merge. Provider mechanism status: claude ✅ confirmed · codex ◐ partially confirmed (`notify`→drain) · gemini ◐ scaffold-with-caveat (hook key unconfirmed) · aider ◐ scaffold-with-caveat (limited hook surface). CLI-only by design (no new standing MCP tool/token). |
| **Backup / restore** (atm-core, WL-035) | ✅ | — | — | — | CLI-only by design (host-local file I/O on a consistent snapshot); MCP does not expose it. |
| **Session export/import** (casr, WL-040) | ✅ | ❌ | — | — | CLI-only by design (host-local file I/O); logical JSON interchange (messages + memory), distinct from the WL-035 binary backup. MCP exposure is a catalog-only follow-up if ever needed (no new standing tool). See `FORMAT-session-export.md`. |
| **Supersede** (atm-core, WL-037) | ✅ | ✅ | ❌ | ❌ | `weave send --supersedes` / `weave_send {supersedes}` (zero standing-token cost — a `weave_send` property, not a new tool). |
| **Post-send hooks** (atm-core, WL-036) | ✅ | ✅ | — | — | Operator config (`[[post_send_hook]]`); fires on send/notify/ack from both agent surfaces. No new standing MCP tool. |

### The headline result

- **CLI and MCP parity is now decision-backed, not asserted by slogan** — CLI remains the
  zero-standing-cost reference path and MCP remains the token-light structured path through
  the `weave` meta-tool catalog. Every CLI command must carry an explicit `mcp_decision`
  (`mcp-catalog`, `mcp-catalog-dangerous`, feature-gated MCP, or a documented CLI-only
  rationale) in the command-surface ledger exposed by `weave tui --json --pane commands`.
  The integration gate compares that ledger exactly to `weave --help`, so new CLI-first work
  cannot bypass the MCP/status decision.
- **Dashboard and bots are deliberately the v1 baseline** (WL-048/WL-073): the dashboard is a read-only
  presence/activity view; the bots relay text (inbound human→agent + outbound notify) and answer
  the first command grammar. They are
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
- **WL-052b — Bot command grammar (DONE by WL-073).** The Telegram and Slack bridges answer
  `/inbox`, `/peers`, `/sessions`, `/help`, plus explicitly gated `/send`, `/ask`, `/answer`, and
  `/reply`, by dispatching through the **same** `dispatch_request` handler as MCP/CLI. Read commands
  run in safe mode; write commands require `WEAVE_BOT_WRITES=1` and then pass the same dangerous-tool
  gate. Pure parser/mapper/formatter, unit-tested.
- **Design law for both:** a human surface must call the **same** capability handler as CLI/MCP —
  parity is achieved by *routing to one implementation*, not by re-implementing per surface. That
  is what keeps this matrix honest and the behavior identical everywhere.
- **WL-042 — multi-provider host wiring (tracked caveats, not silent).** `weave setup --provider`
  now scaffolds Codex/Gemini/Aider lifecycle wiring alongside the confirmed Claude path. Two of
  these mechanisms are **scaffolded with an explicit caveat** and are tracked here rather than
  presented as fully confirmed:
  - **gemini** — Gemini CLI uses a Claude-shaped `~/.gemini/settings.json`, but its **exact
    lifecycle-hook key is unconfirmed**. weave writes the documented best-known (Claude-compatible)
    `hooks.{event}` shape and prints the caveat each run. **Follow-up:** confirm Gemini's hook key
    (or whether it only supports MCP-server registration) and update the writer accordingly.
  - **aider** — Aider's `~/.aider.conf.yml` has **no rich lifecycle-hook surface**. weave appends a
    minimal hand-templated `weave-hook:` stanza (no YAML dependency) that Aider may ignore until it
    grows a hook surface. **Follow-up:** revisit when Aider ships lifecycle hooks; until then the
    stanza is documentation of intent, not a working hook.

## Why this is the right v1 boundary

Agents drive weave through CLI/MCP with explicit parity decisions and token-light discovery. Humans observe through the
dashboard and nudge through chat. Shipping the **read/relay** human surfaces first (WL-048) and
**measuring** the write gap here (WL-052) — rather than rushing write paths into a hand-rolled
HTTP server and a chat parser — keeps the security invariants (no-shell, parameterized SQL,
input caps, destructive-op gating) intact while making the remaining parity work explicit and
routable to a single implementation.
