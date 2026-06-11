---
repo: "Dicklesworthstone/mcp_agent_mail"
url: "https://github.com/Dicklesworthstone/mcp_agent_mail"
language: "python | rust"
last_scanned: "2026-06-07"
scan_agent: "agent-ldnojvn3"
status: "active"
---

# MCP Agent Mail — Feature Inventory

**Elevator pitch:** "Gmail for your coding agents" — asynchronous mail-like coordination
layer for AI coding agents exposed as an MCP server with advisory file leases,
searchable threads, and dual SQLite + Git persistence.

---

## 1. Feature Inventory

### Identities, Inboxes & Threads
- Auto-generated memorable Adjective+Noun agent names (e.g., `GreenCastle`)
- Per-agent inbox/outbox with reverse-chronological fetch, pagination, read/ack tracking
- Threading by `thread_id` with full conversation continuity, subject lines, CC/BCC
- Importance levels: `low`, `normal`, `high`, `urgent`
- `ack_required` on sends with overdue/escalation tracking
- **FTS5-backed full-text search** with query syntax (`"exact phrase"`, `AND`/`OR`/`NOT`, `subject:foo`)
- `summarize_thread` extracts key points and action items
- Read-only MCP resources: `resource://inbox/{Agent}`, `resource://thread/{id}`

### Advisory File Leases (Reservations)
- Reserve file paths or globs (`src/**`) with TTL-based expiry
- Exclusive vs shared reservations
- Conflict detection (`file_reservation_paths`)
- `renew_file_reservations`, `release_file_reservations`, `force_release_file_reservation`
- **Pre-commit guard** — Git hook blocks commits touching exclusively reserved files
- Stale recovery with PID-checked reclaim

### Architecture
- **Dual persistence**: SQLite (WAL, connection pooling, FTS5) + Git-backed markdown archive
- Write pipeline: SQLite first, then Git markdown artifacts
- **Commit coalescer** (Rust): batches rapid-fire writes (9.1x reduction)
- Date-partitioned messages (`messages/YYYY/MM/{id}.md`)
- Search V3: pluggable `frankensearch` — lexical default, semantic + hybrid behind feature gate

### CLI & Surfaces
- `am` operator CLI (Rust) / Python CLI
- Server modes: HTTP + TUI, stdio, `--reuse-running`
- **Robot mode**: 18 non-interactive subcommands (`status`, `inbox`, `timeline`, `search`, etc.)
- **TUI**: 16-screen interactive console (Dashboard, Messages, Threads, Agents, Search, Reservations, etc.)
- `am doctor check/repair/backups/restore`
- Archive/disaster recovery (ZIP snapshots)
- Share & deploy: export to GitHub Pages / Cloudflare Pages with Ed25519 signing, age encryption

### Unique Features
- **Contact policies**: per-agent ACLs (`open`, `auto`, `contacts_only`, `block_all`)
- **Human Overseer**: Web UI compose form for humans to send high-priority messages bypassing policies
- **Related Projects Discovery**: AI-powered suggestions for linking related repos
- **Product Bus**: cross-project inbox/search/summarize for multi-repo agent fleets
- **Build slots**: `acquire_build_slot` for gating expensive CI work
- **Static mailbox export**: self-contained portable HTML bundles with search + crypto
- **ATC learning loop**: durable live-learning with experience rows and replay
- `#![forbid(unsafe_code)]` (Rust)
- No broadcast-by-default (discrete, addressed, threaded like email)
- Semi-persistent identity (agents can vanish without breaking system)

---

## 2. Weave Overlap

| MCP Agent Mail Feature | Weave Equivalent | Notes |
|------------------------|------------------|-------|
| Inbox/outbox | `weave inbox` / `weave outbox` | Parity |
| Threading | `weave thread` | Parity |
| Search | — | **Gap**: weave has no FTS search |
| Ack tracking | `weave ack` / `weave ask` | Parity |
| File reservations | — | **Gap**: no advisory leases |
| Pre-commit guard | — | **Gap**: no Git hooks for reservations |
| Contact policies / ACLs | — | **Gap**: no per-peer ACLs |
| Human Overseer surface | — | **Gap**: no human web UI |
| Summarize thread | — | **Gap**: no LLM summarization |
| Importance levels | — | **Gap**: no message priority |
| Static export | — | **Gap**: no mailbox export |

---

## 3. Weave Gaps

### High Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **FTS5 full-text search** | weave messages are unsearchable beyond basic listing; agents need to find old messages |
| 2 | **Advisory file leases** | Prevents conflicting edits between agents; reservation + pre-commit guard is a proven pattern |

### Medium Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 3 | **Message importance / priority** | Urgent messages should cut through noise |
| 4 | **Contact policies / per-peer ACLs** | Currently any peer in circle can message any other; no blocking/handshake |
| 5 | **Thread summarization** | Long threads need distilled summaries for context windows |
| 6 | **Static mailbox export** | Human auditability, compliance, sharing |

### Low Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 7 | **Human Overseer web UI** | Human-in-the-loop messaging; out of scope for minimal CLI tool |
| 8 | **Product Bus / cross-project** | Multi-repo fleets; weave's federation is store-level, not semantic |
| 9 | **Build slots** | CI gating; niche use case |
| 10 | **ATC learning loop** | Over-engineered for weave's scope |

---

## 4. Proposed WL Items

- `WL-028` — FTS5 full-text search on messages, threads, and subjects
- `WL-029` — Advisory file leases with TTL expiry and conflict detection
- `WL-030` — Pre-commit Git hook for file reservation guard
- `WL-031` — Message importance / priority levels with urgent filtering
- `WL-032` — Per-peer contact policies (open / auto / contacts_only / block_all)
- `WL-033` — Thread summarization via LLM integration
- `WL-034` — Static mailbox export (HTML bundle with search)

---

## 5. Integration Opportunities

- MCP Agent Mail could use weave's native zellij/kitty/wezterm/screen injector instead of its tmux-only nudge
- weave could pull from MCP Agent Mail's SQLite as a `WEAVE_PEER_DB`
- Both use SQLite + Git; data portability is feasible

---

## 6. Notes

- MCP Agent Mail is significantly larger in scope than weave (37 MCP tools, 16 TUI screens, web UI, etc.)
- The Rust rewrite (`#![forbid(unsafe_code)]`, structured concurrency) is architecturally impressive
- weave should adopt the **file lease** and **FTS search** patterns, not the full TUI/dashboard stack
