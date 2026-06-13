# weave — Product Requirements Document

**Status:** v0.2.0 — delivered and shipping. weave is a working Rust-native
agent-to-agent **orchestration mesh** in one dependency-light static binary:
**70 `weave_*` MCP tools** plus full CLI parity, a `Store` trait with BOTH
**sqlite** (default, bundled) and **libSQL/Turso** (feature) backends, optional
`sign` (ed25519) and `llm` (thread summarization) features, a native injector
for **five muxes** (tmux, zellij, kitty, wezterm, screen) plus iTerm2,
lifecycle-hook auto-delivery, `weave setup` automation, cross-store federation +
Tier-2 pull, a CI gate of six required checks across both backends (≈531 sqlite
/ ≈491 libsql tests), and the binary shipped in the RTX-5090 wizard image.
**Owner:** drdave
**One-liner:** The definitive **Rust-native superset of repowire** — a full
agent-to-agent orchestration mesh (messaging + native pane injection, structured
asks/answers/permissions, a durable job board, leases, orchestrator turn-state,
a review queue, scheduling, agent memory, signing, and LLM summarization) in one
dependency-light binary, **Python-free, no daemon** (the DB *is* the broker).

---

## 0. North star (owner-corrected 2026-06-13)

weave is the **DEFINITIVE Rust-native SUPERSET of repowire — MORE than repowire,
not less** — in one dependency-light static binary, Python-free. It is a full
agent-to-agent **orchestration mesh**, not merely a messenger. The early framing
in this PRD ("let sessions message each other and inject into a pane") described
the v0.1.0 *seed*; the binary has since grown the whole mesh. Three properties
are non-negotiable and shape every requirement below:

- **dependency-light** — one small static binary; no Python, no runtime venv;
  heavyweight deps (libSQL/tokio tree, the LLM client) live behind feature flags.
- **no-daemon** — there is no relay process; the SQLite/libSQL file is the
  broker, and any mux CLI can push into any pane, so the *sender* injects.
- **token-light** (ADR-0003, new first-class invariant, peer of
  dependency-light) — the standing context cost of the agent surface must stay
  small regardless of how many capabilities exist; adding a feature must not add
  standing tokens.

## 1. Problem

Claude Code sessions are isolated: no built-in way for one session to message,
coordinate with, or orchestrate another. Two prior attempts on this box:

1. **`mcp-broker`** (Python + libSQL) — works, but **poll-only**: the recipient
   only sees a message when it calls `broker_inbox`. No push; a running session
   is never flagged.
2. **`repowire`** (Python) — does push via a daemon + peer registry + **tmux**
   pane injection, and grew a broad orchestration surface (asks, job board,
   orchestrator role, presence, dashboard/Telegram/Slack). But: it is
   **tmux-first with no native zellij injector**, it is **Python** (a runtime +
   venv to manage), it relies on a **long-running daemon**, and a documented
   bracketed-paste bug let a naïve Enter cancel a TUI mid-tool-call.

This box's daily shell is **zellij** (via yazelix). Neither prior tool gives
clean, native, zellij-aware push, and neither does it in a single dependency-free
binary. weave's mandate is to deliver **everything repowire does and more**, in
Rust, without the daemon, the runtime, or the paste bug.

## 2. Vision

**weave** is a single static Rust binary that is the **agent orchestration mesh**
for a fleet of coding-agent sessions:

- Gives every agent session a **name**, a persistent **mailbox**, and a place in
  a **circle** (the mesh).
- **Pushes** new messages into a running session's pane via a **native injector**
  that speaks **tmux, zellij, kitty, wezterm, and screen** (plus iTerm2),
  **paste-safe** per mux, with no Python and no reliance on repowire.
- Falls back to **hook-driven delivery-on-next-turn** when no multiplexer is
  present, so it degrades gracefully everywhere.
- Is **MCP-native**: exposes **70 `weave_*` tools** over stdio (and an optional
  HTTP surface) so any agent can drive the full mesh without shelling out — with
  **full CLI parity** as the zero-standing-token-cost path.
- Coordinates real work: **structured asks/answers/acks**, **broadcast** and
  **ask-many** fan-out, **tool-permission gating**, a **durable job board**,
  **advisory leases**, **orchestrator turn-state**, a **review queue**,
  **scheduling**, **agent memory**, optional **ed25519 signing**, and optional
  **LLM thread summarization**.
- Stores state in a **libSQL-compatible SQLite** file (no daemon; the DB is the
  broker), with optional **cross-store federation** and **Tier-2 pull** between
  stores/machines.

The mission is **superset, not subset**: every repowire capability is present or
exceeded, and the gaps that remain (below) are explicitly in scope, not dropped.

## 3. Goals / Non-goals

### Goals
- G1. One self-contained binary; no Python, no runtime venv; **dependency-light**.
- G2. **Native injector** for tmux + zellij + kitty + wezterm + screen (+ iTerm2);
  pluggable trait for more muxes; paste-safe submission per mux.
- G3. MCP stdio server exposing the **full mesh** (70 tools) with the proven broker
  semantics (per-reader read tracking, broadcast, history, sessions,
  clear-with-confirm) **plus** the orchestration surface (asks, jobs, leases,
  orchestrator, reviews, scheduling, memory, permissions, summarization).
- G4. Claude Code lifecycle hooks: auto-register on `SessionStart`, auto-deliver on
  `UserPromptSubmit`/`Stop`, optional blocking `Stop`/`SubagentStop` wake.
- G5. Graceful degradation: no mux → next-turn delivery; mux missing → clear
  message, never crash; no browser/peer present → mesh still works.
- G6. libSQL/Turso backend (clean path to sync/replicas) **and** cross-store
  federation + Tier-2 cross-machine pull, both behind the same `Store` trait.
- G7. **token-light** (ADR-0003): keep every feature on every surface while the
  standing MCP cost stays bounded (progressive disclosure); CLI is the
  zero-standing-cost path.

### Non-goals (for now)
- A long-running daemon as the *transport*. The store + mux CLIs are the push
  path; the **optional** `weaved` presence daemon only tracks online/offline +
  lifecycle eviction — it is never required for delivery.
- Linking a browser engine (V8) into weave's core. Web reach is delivered by
  **governing** the separate `obscura-mcp` binary (ADR-0002), not by embedding it.
- Re-introducing a Python runtime or a Next.js human surface. The dashboard /
  Telegram / Slack surfaces return (WL-048) **Rust-native and feature-flagged**.

## 4. Architecture

### Current (v0.2.0) — interim Cargo workspace, one binary
```
weave-core/   library: model + config + Store trait + both backends + memory + sign + llm
weave-inject/ library: native multi-mux injector (pure command tables + runner)
weave-mcp/    library: MCP stdio JSON-RPC 2.0 server (70 weave_* tools) + optional HTTP surface
weave/        binary: clap CLI (full parity) + setup + hooks + git tagging + harness
```
Strictly layered — `weave ▸ weave-mcp ▸ {weave-inject ▸} weave-core`, no upward
deps (compiler-enforced). The default build still produces **one** static binary
(`target/release/weave`). **The four-crate split is interim; single-crate remains
the structural goal** (collapse after the meta workspace is aligned — WL-043).
Deep detail is in `ARCHITECTURE.md`.

### How push works (no daemon)
`tmux send-keys -t <pane>`, `zellij --session <name> action write-chars`,
`kitten @ send-text`, `wezterm cli send-text`, `screen -X stuff` each reach an
arbitrary pane/session from any process, so the **sender** injects directly into
the recipient's registered pane. The `peers` table maps `name → (mux, pane/session
id)`, captured from env at `SessionStart`. Submission is **paste-safe per mux**
(bracketed-paste close via hex `ESC[201~` for tmux — the repowire bug this fixes).

## 5. Data model (selected)
- `messages(id, ts, sender, recipient, subject, body, priority, …)` — append-only.
- `reads(message_id, reader, ts)` — per-reader read state (broadcast delivered once
  per reader).
- `peers(name, mux, target, cwd, last_seen, turn_state, description, contact_policy,
  birth_cert, …)` — the injection + presence + identity registry.
- `asks` / `ask_groups` — tracked ask/answer/ack + ask-many correlation.
- `jobs` — durable poll-only job board (create/claim/update/result/cancel).
- `leases` — advisory path leases with TTL + conflict detection.
- `review_queue` — PR review state across peers.
- `schedules` — one-shot + recurring scheduled deliveries.
- `summaries` — cached LLM thread summaries.
- `outbox` / Tier-2 `Intent` — cross-store delivery with idempotency keys + trace IDs.
- memory: filesystem-backed scoped store under `~/.config/weave/memory/`.

## 6. Agent surface (MCP + CLI parity)
**70 `weave_*` MCP tools** spanning: messaging + inject (`weave_send`,
`weave_notify`, `weave_inbox`, `weave_history`, `weave_thread`, `weave_reply`,
`weave_scan`, `weave_search`, `weave_clear`); peers/presence (`weave_peers`,
`weave_sessions`, `weave_connect`, `weave_whoami`, `weave_set_turn_state`,
`weave_set_description`, `weave_set_peer_policy`/`get`); asks/permissions
(`weave_ask`, `weave_answer`, `weave_ack`, `weave_asks`, `weave_ask_many`,
`weave_ask_permission`, `weave_permission_*`); broadcast (`weave_broadcast_notify`,
`weave_broadcast_ask`); orchestrator (`weave_claim_orchestrator`,
`weave_orchestrator_status`); job board (`weave_job_*`); leases (`weave_lease_*`);
review queue (`weave_review_*`); scheduling (`weave_schedule`, `weave_schedules`,
`weave_cancel_schedule`, `weave_tick`); memory (`weave_memory_*`); summarization
(`weave_thread_summarize`, `weave_summarize_text`); daemon (`weave_daemon_*`);
priority (`weave_set_message_priority`); admin (`weave_setup`, `weave_uninstall`,
`weave_doctor`). Every capability has a **CLI** equivalent — the zero-standing-
token path (`rtk weave …` for compressed output), per ADR-0003.

## 7. Lifecycle hooks (Claude Code)
- `SessionStart` → `weave hook session` → register peer (name + pane + birth cert).
- `UserPromptSubmit` → `weave hook prompt` → drain unread to stdout; resurface
  unacked asks as content-free nudges.
- `Stop` → `weave hook stop` → drain; optional blocking `--wake` returns
  `additionalContext` to drive the next turn without polling (WL-025).
- `weave tick` (and the prompt hook) fire due scheduled deliveries.

## 8. repowire-superset framing (honest, with the in-scope gaps)

weave **supersets** repowire on the local agent mesh: messaging + push (native,
multi-mux, paste-safe vs. tmux-only), structured asks/answers/acks, broadcast and
ask-many, tool-permission gating, presence + turn-state, circles + orchestrator
role, durable job board, leases, review queue, scheduling, agent memory, plus
extras repowire lacks (ed25519 signed identity, LLM summarization, cross-store
federation + Tier-2 pull, FTS search). The provable parity matrix is
`docs/REPOWIRE-PARITY.md` (**WL-046**).

The remaining mission gaps — **in scope, not dropped**:
- **Agent spawn/kill** (`weave_spawn_peer`/`weave_kill_peer`, argv-only, per-mux,
  birth-cert identity) — repowire parity weave currently lacks — **WL-047**.
- **Rust-native human surfaces** — repowire's dashboard / Telegram / Slack, but
  **Rust-native, no Next.js/Python**, over `weave-mcp/http.rs`, behind a
  `--features surfaces` flag so the default build stays lean — **WL-048 / WL-052**.
- **Governed web reach** — close the mesh's web/network weakness via **obscura**
  (separate `obscura-mcp` binary) registered as a weave-governed capability
  (permission/lease/job-gated stealth browsing); **NO V8 in weave's core** —
  **WL-049**, decided in **ADR-0002** (`.handoff/decisions/ADR-0002-…`).
- **token-light surface** — replace the 70 eager flat MCP tools with
  progressive-disclosure dispatchers/meta-tool (≤ ~2k standing tokens, zero
  capability loss); add `token-light` as a guarded invariant — **WL-050..052**,
  decided in **ADR-0003** (`.handoff/decisions/ADR-0003-…`).

## 9. Milestones
- **M0–M3 (done):** store + native injector (5 muxes) + MCP server + CLI + hooks +
  `weave setup` + workspace split + libSQL backend + presence daemon + the full
  orchestration surface (asks, jobs, leases, orchestrator, reviews, scheduling,
  memory, permissions, sign, summarization) — WL-001..033 merged.
- **M4 — repowire-superset completion:** parity audit (WL-046), agent spawn/kill
  (WL-047), Rust-native human surfaces (WL-048), obscura governance (WL-049).
- **M5 — token-light:** progressive-disclosure MCP (WL-050), invariant + budget
  gate (WL-051), full multi-surface parity (WL-052).
- **Structural:** collapse the interim 4-crate workspace back to single-crate
  after the meta workspace is aligned (WL-043).

## 10. Comparison
| | mcp-broker | repowire | **weave** |
|---|---|---|---|
| Language | Python | Python | **Rust (1 binary)** |
| Push to running session | ❌ poll only | ✅ tmux | ✅ **tmux + zellij + kitty + wezterm + screen + iTerm2 (native, paste-safe)** |
| Daemon required | no | **yes** | **no** (optional presence daemon only) |
| MCP-native | ✅ | ✅ | ✅ **70 tools + full CLI parity** |
| Orchestration (asks/jobs/leases/orchestrator/reviews/schedule) | ❌ | ✅ | ✅ **superset** |
| Agent memory / signing / LLM summary / FTS | ❌ | partial | ✅ |
| Cross-store federation / Tier-2 pull | ❌ | relay-based | ✅ **no-daemon** |
| Agent spawn/kill | ❌ | ✅ | ⏳ **WL-047 (in scope)** |
| Human surfaces (dashboard/TG/Slack) | ❌ | ✅ (Python/Next.js) | ⏳ **WL-048 (Rust-native, in scope)** |
| Governed web access | ❌ | hosted relay | ⏳ **WL-049 / ADR-0002 (obscura, no V8 in core)** |
| Token-light surface | n/a | n/a | ⏳ **WL-050..052 / ADR-0003** |
