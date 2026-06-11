---
repo: "prassanna-ravishankar/repowire"
url: "https://github.com/prassanna-ravishankar/repowire"
language: "python"
last_scanned: "2026-06-07"
scan_agent: "agent-ny4l8i76"
status: "active"
---

# Repowire — Feature Inventory

**Elevator pitch:** Local-first mesh network for AI coding agents — a control plane
around agent sessions you already have open, with tmux-centric pane management
and optional hosted relay for remote access.

---

## 1. Feature Inventory

### Messaging Primitives
- `ask` — non-blocking question with explicit `ack` reply; returns `correlation_id` immediately
- `ack` — closes an open ask thread; replies always reach original asker regardless of circle
- `answer` — typed question envelopes (choice, free text, tool-permission)
- `notify_peer` — fire-and-forget message with synthetic tracking ID
- `broadcast` — fan-out to all online peers in caller's circle
- `ask_many` / `ask_many_result` — ask same question to N peers in parallel under parent ID
- **Reminder injection** — unacked asks resurface as reminder blocks at every subsequent prompt
- **Structured tool-approval questions** — blocking choice questions for tool permissions

### Session / Peer Lifecycle
- `spawn_peer` — spawn a new peer in a tmux pane/window
- `kill_peer` — kill a peer's pane/session
- `peer restart` / `session resume` — backend-native resume with pre-validation
- **Birth certificates** — unguessable nonces minted at SessionStart to prevent identity takeover
- **Lazy repair** — no polling; request-driven reconciliation with bounded cooldowns

### Presence & Liveness
- Peer registry with `peer_id`, `name`, `circle`, `status`, `turn_state`, `last_seen`
- **Contradiction events** — `ONLINE_BUT_NO_WS`, `PANE_MISSING`, `AGENT_PID_DEAD`
- `orchestrator_status` — confirm live orchestrator before dispatch

### Scheduler & Jobs
- `schedule_create` / `schedule_self` / `schedule_cron` — one-shot and recurring messages
- `job_create` / `job_run` / `job_retry` / `job_cancel` — durable tracked work with continuity modes
- `agents create` — scaffold repo-local agent folders (`AGENTS.md` / `CLAUDE.md`)

### Memory & Context
- `memory path/list/show/search/write` — filesystem-backed mesh memory under `~/.repowire/memory/`
- **Orchestrator recall** — daemon-side lexical scan of workspace memory prefixes inbound messages
- **Persona system (`SOUL.md`)** — orchestrator identity files with precedence-based resolution

### Human Surfaces
- **Browser dashboard** (Next.js)
- **Telegram bot**
- **Slack bot**

### Security
- Bearer token auth for local HTTP / WebSocket / hooks
- Spawn allowlists (`daemon.spawn.allowed_paths`)
- **PreToolUse tool approval** — gates mutating tools behind blocking approval
- CORS restricted to localhost + relay
- No end-to-end encryption of relay tunnel payloads (acknowledged gap)

### Transport & Federation
- Default stdio MCP server
- Experimental Streamable HTTP MCP (localhost-only, bearer auth)
- Optional hosted relay at `repowire.io` (outbound WSS, no inbound ports)
- Self-hosted relay option
- API-key scoped relay isolation

---

## 2. Weave Overlap (already implemented)

| Repowire Feature | Weave Equivalent | Notes |
|------------------|------------------|-------|
| `ask` / `ack` | `weave ask` / `weave ack` | Parity |
| `answer` | `weave answer` | Parity |
| `notify_peer` | `weave notify` | Parity |
| `broadcast` | `weave send --to all` | Parity |
| `ask_many` | `weave ask_many` | Parity |
| `ask_many_result` | `weave ask_many_result` | Parity |
| `list_peers` / `whoami` | `weave peers` / `weave scan` | Parity |
| `set_description` | `weave describe` | Parity |
| `turn_state` | `weave status` + hooks | Parity |
| `claim_orchestrator_role` | `weave orchestrator claim` | Parity |
| `orchestrator_status` | `weave orchestrator status` | Parity |
| `job_create/list/status/update/result/cancel` | `weave job create/list/show/claim/update/result/cancel` | Parity |
| `delivery trace` | `weave delivery` + `delivery_log` table | Parity |
| SQLite state store | SQLite / libSQL backends | weave has MORE backends |
| `doctor` | `weave doctor` | Parity |
| `setup` / `uninstall` | `weave setup` / `weave uninstall` | Parity |
| Lifecycle hooks | `weave hook session/prompt/stop/wake` | Parity |
| MCP server | `weave mcp` (stdio) | Parity |

---

## 3. Weave Gaps (ranked by impact)

### High Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **Reminder injection for open asks** | Unacked asks silently stall; reminders keep them alive in the recipient's workflow |
| 2 | **Structured question types** | Plain-string asks are weak; choice/tool-permission questions are actionable |

### Medium Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 3 | **Scheduler / cron for messages** | Periodic tasks, reminders, recurring reports |
| 4 | **Mesh memory system** | Persistent context scoped by project/persona/orchestrator |
| 5 | **Birth certificates / runtime identity envelopes** | Prevents session-id squatting and path-based takeover |
| 6 | **Co-orchestrator support** | Resilience when one orchestrator hits rate limits |

### Low Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 7 | **GitHub review queue integration** | Nice-to-have for multi-agent review workflows |
| 8 | **PreToolUse tool approval** | Security gate; less critical in single-user model |
| 9 | **Streamable-HTTP MCP transport** | Enables remote agents; blocked by network-exposure non-goal |
| 10 | **Dashboard UI (Next.js)** | weave is CLI-first by design; `sessions --watch` covers 80% |
| 11 | **Telegram / Slack bot peers** | Human surfaces; out of scope for minimal tool |
| 12 | **Spawn / kill peer** | Requires daemon; weave is daemon-optional by design |

---

## 4. Proposed WL Items

All gaps above are tracked in the main backlog as:
- `WL-014` — Reminder injection for open asks
- `WL-015` — Structured question types
- `WL-016` — Scheduler / cron for messages
- `WL-017` — Mesh memory system
- `WL-018` — Birth certificates / runtime identity envelopes
- `WL-019` — Co-orchestrator support
- `WL-020` — GitHub review queue integration
- `WL-021` — PreToolUse tool approval
- `WL-022` — Streamable-HTTP MCP transport
- `WL-023` — iTerm2 injector backend (already on ROADMAP-v0.3)
- `WL-024` — Reservation leases (already on ROADMAP-v0.3)
- `WL-025` — Stop-boundary wake (already on ROADMAP-v0.3)
- `WL-026` — Idempotency keys & trace IDs (already on ROADMAP-v0.3)
- `WL-027` — Broadcast notify / broadcast ask

---

## 5. Integration Opportunities

Weave does NOT need to replace repowire. They serve different deployment models:
- **repowire** = Python-heavy, tmux-first, relay-centric, dashboard-driven
- **weave** = Rust-native, 5-mux native, local-first, CLI-first, daemon-optional

Integration ideas:
- weave could pull from repowire's SQLite state.db as a `WEAVE_PEER_DB` (Tier-1 federation)
- repowire could register as a weave peer via its stdio MCP (if repowire exposes one)
- Both tools could share the same `ZELLIJ_PANE_ID` / tmux pane conventions

---

## 6. Notes

- **Security:** repowire acknowledges no E2E encryption on relay tunnels; weave avoids
  relay entirely by using cross-store pull (owner-only writes). This is an architectural
  win for weave on sensitive workloads.
- **Performance:** repowire's "lazy repair" (no polling) is elegant. weave's optional
  daemon polls every 15s when running, but degrades gracefully to TTL-only when stopped.
  A future optimization could adopt request-driven reconciliation.
- **Non-goal:** Converting weave into a Python/FastAPI dashboard tool. The gaps to adopt
  are messaging primitives and reliability features, not the deployment stack.
