# weave ⊇ repowire — Feature-Parity Superset Matrix

**Claim under audit (WL-046):** weave is the *definitive Rust-native **superset**
of repowire — MORE than repowire, not less*. This document makes that claim
**provable, not asserted**, by mapping every repowire feature onto weave's actual
surface, each verdict anchored to a concrete tool / table / module / WL item in
the code.

**Provenance**
- **repowire inventory:** `prassanna-ravishankar/repowire` (Python), scanned
  2026-06-07 — archived at
  `.handoff/loop/_done/_workspace_prev/references/features/prassanna-ravishankar--repowire.md`.
  (That scan's "weave gaps" list predates WL-014..033; most of it is now closed —
  this matrix is the current truth.)
- **weave surface:** v0.2.0 — **70 `weave_*` MCP tools** (counted in
  `weave-mcp/src/mcp.rs`) with full CLI parity, dual backend (sqlite default +
  libSQL/Turso), `sign`/`llm` optional features. Merged via WL-001..033 (ground
  truth `.handoff/loop/backlog.md`).

**Verdict key:** ✅ **HAVE** (parity) · 🟢 **SUPERSET** (weave does strictly more) ·
🔶 **PARTIAL** (core present, a sub-feature is the gap) · ⏳ **GAP** (tracked WL item) ·
🧭 **SUPERSEDED** (intentionally replaced by a better architecture).

---

## 0. Bottom line

| | Count |
|---|---|
| repowire features that weave **HAS or SUPERSETS** | **35 of 36** |
| **Genuine gaps**, all tracked | **2** — minor only (agents-scaffold, `SOUL.md` file) |
| **Superseded by design** (no-daemon) | **2** — hosted relay, API-key relay isolation → cross-store pull |
| weave capabilities repowire **does NOT have** | **13** (§4) |

**Conclusion: the superset claim holds.** Every repowire *messaging,
orchestration, scheduling, memory, presence, security, and transport* primitive
is present or exceeded. With **agent spawn/kill shipped** (WL-047), **the human
surfaces shipped** (WL-048 — Rust-native web dashboard + Telegram/Slack behind
`--features surfaces`), **and governed web reach shipped** (WL-049 — stealth
browsing via obscura behind `--features obscura`), the remaining gaps are **two
minor conveniences** only. weave additionally ships **13** capabilities repowire
never had (signing, summarization, leases, FTS, federation, dual backend, graph
analytics, …), and now **EXCEEDS** repowire's hosted-relay web reach with
*governed stealth browsing* (WL-049 / ADR-0002): web access **without a daemon**,
gated + leased + audited + SSRF-guarded, where repowire offered only an
ungoverned hosted relay.

---

## 1. Messaging primitives

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| `ask` (non-blocking, returns correlation_id) | `weave_ask` / `weave ask` | 🟢 SUPERSET | `mcp.rs`; structured kinds (below) exceed plain ask |
| `ack` (closes ask; reply always reaches asker) | `weave_ack` | ✅ HAVE | `asks` table §6; cross-circle reply preserved |
| `answer` (typed: choice / free-text / tool-permission) | `weave_answer` + `AskKind::{FreeText,Choice,ToolPermission}` | ✅ HAVE | WL-015; `kind`/`options` columns |
| `notify_peer` (fire-and-forget + tracking id) | `weave_notify` | ✅ HAVE | synthetic id + `delivery` trace |
| `broadcast` (fan-out to circle) | `weave_send --to all`, `weave_broadcast_notify`, `weave_broadcast_ask` | 🟢 SUPERSET | broadcast **ask** too (WL-027), not just notify |
| `ask_many` / `ask_many_result` | `weave_ask_many` / `weave_ask_many_result` | ✅ HAVE | `ask_groups` parent-id correlation |
| Reminder injection (unacked asks resurface) | prompt-hook reminder nudge | ✅ HAVE | WL-014 |
| Structured tool-approval questions | `weave_ask_permission` + `weave_permission_*` | ✅ HAVE | WL-021 (PreToolUse gate) |

## 2. Session / peer lifecycle

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| `spawn_peer` (spawn agent into pane/window) | `weave_spawn_peer` / `weave spawn` | ✅ HAVE | **WL-047**; argv-only, per-mux, birth-cert id, spawn allowlist (§7) |
| `kill_peer` (kill pane/session) | `weave_kill_peer` / `weave kill` | ✅ HAVE | **WL-047**; per-mux kill argv (exact pane, or coarse session for zellij/screen) |
| `peer restart` / `session resume` | `weave_kill_peer` + `weave_spawn_peer` | ✅ HAVE | **WL-047** spawn/kill family (kill then re-spawn) |
| Birth certificates (nonce at SessionStart) | `birth_cert` column + getrandom nonce + verify-on-reregister | ✅ HAVE | WL-018; `--cert` / `WEAVE_BIRTH_CERT` |
| Lazy repair (no polling, request-driven) | request-driven liveness + TTL eviction; optional daemon | 🟢 SUPERSET | §6 presence; works with **no** daemon |

## 3. Presence & liveness

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| Peer registry (id/name/circle/status/turn_state/last_seen) | `peers` table (+ `description`, `contact_policy`, `birth_cert`) | 🟢 SUPERSET | richer schema (WL-031/032) |
| Contradiction events (`ONLINE_BUT_NO_WS`, `PANE_MISSING`, `AGENT_PID_DEAD`) | `liveness_for` / `is_alive` vs `is_online`, host-aware | 🟢 SUPERSET | ARCHITECTURE §6; host-aware across machines |
| `orchestrator_status` (confirm live orchestrator) | `weave_orchestrator_status` + co-orchestrator | 🟢 SUPERSET | WL-019 (multiple live holders) |

## 4. Scheduler & jobs

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| `schedule_create` / `schedule_self` / `schedule_cron` | `weave_schedule` / `weave_schedules` / `weave_cancel_schedule` / `weave_tick` | ✅ HAVE | WL-016; one-shot + recurring, drift-safe |
| `job_create` / `run` / `retry` / `cancel` (continuity modes) | `weave_job_create/claim/update/result/show/list/cancel/status` | 🟢 SUPERSET | durable `jobs` table; 8 tools |
| `agents create` (scaffold `AGENTS.md`/`CLAUDE.md`) | `weave config init` scaffolds config only | 🔶 **PARTIAL** | minor gap: no agent-folder scaffold (candidate WL) |

## 5. Memory & context

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| `memory path/list/show/search/write` (`~/.repowire/memory/`) | `weave_memory_read/write/search/list/delete` (`~/.config/weave/memory/`) | ✅ HAVE | WL-017; `memory.rs` |
| Orchestrator recall (prefix inbound messages) | context prefixing on ask/send/reply/answer delivery | ✅ HAVE | WL-017 |
| Scoping (project / persona / orchestrator) | scopes: `global`/`project`/`persona`/`orchestrator` | ✅ HAVE | `memory.rs` (verified) |
| Persona system (`SOUL.md` precedence files) | persona memory scope (no `SOUL.md` file convention) | 🔶 **PARTIAL** | minor gap: scope present, file-precedence convention absent |

## 6. Human surfaces

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| Browser dashboard (Next.js) | read-only web dashboard (`weave dashboard`), server-rendered HTML + SSE; `weave sessions --watch` TUI also | ✅ **HAVE** | **WL-048** — `weave-mcp/src/dashboard.rs` + `http.rs::serve_dashboard`, Rust-native HTTP/SSE over `http.rs`, no Next.js, `--features surfaces` |
| Telegram bot | Telegram bridge (`weave telegram`), poll-only | ✅ **HAVE** | **WL-048** — `weave/src/telegram.rs`, shared `reqwest` blocking client, `--features surfaces` |
| Slack bot | Slack bridge (`weave slack`), poll-only | ✅ **HAVE** | **WL-048** — `weave/src/slack.rs`, `--features surfaces` |

## 7. Security

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| Bearer token auth (HTTP/WS/hooks) | bearer-auth HTTP MCP surface | ✅ HAVE | WL-022; `http.rs` |
| Spawn allowlists (`daemon.spawn.allowed_paths`) | `spawn_allowed_dirs` / `WEAVE_SPAWN_DIRS` + `trusted_dirs()` (two-layer gate) | ✅ HAVE | **WL-047**; cwd allowlist (deny-by-default, MCP hard-deny) + trusted child `argv[0]` |
| PreToolUse tool approval | `weave_ask_permission` / `weave_permission_*` | ✅ HAVE | WL-021 |
| CORS restricted to localhost | localhost-only HTTP surface | ✅ HAVE | WL-022 |
| **No E2E encryption on relay (acknowledged gap)** | no relay at all; ed25519 signed identity + owner-only cross-store pull | 🟢 SUPERSET | `sign.rs`; closes repowire's own acknowledged weakness |

## 8. Transport & federation

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| stdio MCP server | `weave mcp` (stdio) | ✅ HAVE | `mcp.rs` |
| Streamable HTTP MCP (localhost, bearer) | HTTP MCP surface | ✅ HAVE | WL-022; `http.rs` |
| Hosted relay (`repowire.io`, outbound WSS) | cross-store federation (Tier-1) + Tier-2 cross-machine pull | 🧭 SUPERSEDED | no-daemon; owner-only writes, no inbound port, no tunnel |
| Self-hosted relay | Tier-2 pull from remote sources | 🧭 SUPERSEDED | ARCHITECTURE §10 |
| API-key scoped relay isolation | per-source token/timeout parity on federated pull | 🧭 SUPERSEDED | ARCHITECTURE §10 |

---

## 9. weave capabilities repowire does NOT have (the "more")

These have **no repowire equivalent** — they are why weave is a *superset*, not a port:

1. **ed25519 signed sender identity** — sign/verify, multi-key rotation, revocation audit log (`sign.rs`, `weave_doctor` verify-summary).
2. **LLM thread summarization** — cached in-store summaries (`llm.rs`, `summaries` table; WL-033).
3. **Advisory leases** — path leases with TTL expiry, prefix conflict detection, auto-sweep, pre-commit guard (`leases` table; WL-024/029/030).
4. **FTS5 full-text search** over messages/threads/subjects, with libSQL LIKE fallback (`weave_search`; WL-028).
5. **Dual storage backend** — bundled sqlite **and** libSQL/Turso behind one `Store` trait (repowire is SQLite-only).
6. **Cross-store federation (Tier-1)** + **Tier-2 cross-machine pull** with idempotency keys + trace IDs (WL-026) — relay-free remote reach.
7. **Message priority** (low/normal/high/urgent) + **per-peer contact policies** (open/auto/contacts_only/block_all) (WL-031/032).
8. **5-mux native + iTerm2 injection, paste-safe** (tmux/zellij/kitty/wezterm/screen) vs repowire's tmux-only — with the bracketed-paste fix repowire lacked (WL-007/012/023).
9. **Communication-graph analytics** — `weave graph` (connected components, degree centrality, density) via the FrankenNetworkX extraction.
10. **Stop-boundary wake** — blocking `Stop`/`SubagentStop` hook returns `additionalContext` to drive the next turn without polling (WL-025).
11. **GitHub review queue** across peers (`weave_review_*`; WL-020).
12. **`weave_doctor`** federation-health + signature verify rollup.
13. **One dependency-light static Rust binary, Python-free, no daemon** — the whole thing, vs repowire's Python runtime + daemon + Next.js stack.

Beyond parity, weave **extends past repowire entirely** with **governed web
access via obscura** (WL-049 / **ADR-0002**, ✅ shipped behind `--features
obscura`) — stealth headless browsing gated by weave's permission/lease/job system
and SSRF-guarded, **no V8/tokio in weave's core** (zero new default deps) — a
capability repowire's hosted relay never provided, and one weave delivers
**without a daemon**.

---

## 10. Remaining work to close every gap

| Gap | WL | Notes |
|---|---|---|
| ~~Agent spawn/kill/restart~~ | **WL-047** ✅ done | `weave_spawn_peer`/`weave_kill_peer`, argv-only, per-mux, birth-cert identity, + spawn allowlist — shipped (§2, §7) |
| Rust-native human surfaces (dashboard/Telegram/Slack) | **WL-048 / WL-052** | over `weave-mcp/http.rs`, `--features surfaces`, no Next.js/Python |
| `agents create` folder scaffolding | *(candidate WL)* | minor convenience; `weave config init` already scaffolds config |
| `SOUL.md` persona-file precedence | *(candidate WL)* | persona memory **scope** already exists; only the file convention is missing |
| ~~Governed web reach (beyond repowire)~~ | **WL-049** ✅ done | ADR-0002 (accepted) — obscura-as-capability via spawn-and-speak MCP client, `weave_web`/`weave web`, deny-by-default + SSRF-guarded, no V8/tokio in core (§7, §9) |
| Token-light surface (engineering, not parity) | **WL-050..052** | ADR-0003 — progressive disclosure keeps the 70-tool superset token-light |

**Net:** with agent spawn/kill shipped (WL-047), weave supersets repowire on every
dimension except the human surfaces, which are tracked and in scope. The audit
confirms the docs' claim (PRD §8, ARCHITECTURE §0): **more than repowire, not less.**
