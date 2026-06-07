---
repo: "randlee/atm-core"
url: "https://github.com/randlee/atm-core"
language: "rust"
last_scanned: "2026-06-07"
scan_agent: "agent-mu1x4lfx"
status: "active"
---

# ATM (Agent Team Mail) — Feature Inventory

**Elevator pitch:** Local CLI and core Rust library for agent-to-agent mailbox workflows
within Claude Code team directories (`~/.claude/teams`). Daemon-free, cross-platform.

---

## 1. Feature Inventory

### Team Management
- `atm teams` — list discovered teams
- `atm members <team>` — show roster
- `atm teams backup <team>` — timestamped snapshots of config, inboxes, task state
- `atm teams restore <team> --from <snapshot>` — restore with dry-run support
- `atm teams add-member <team> <agent>` — local roster repair
- **Baseline roster enforcement** — `.atm.toml` `[atm].team_members` vs runtime `config.json`

### Messaging
- `atm send <agent[@team]> "<message>"` with `--requires-ack`, `--task-id`, `--file`, `--stdin`, `--summary`
- **Cross-team messaging** — `agent@other-team` syntax
- `atm read` — one full actionable message (prioritizes pending-ack over unread)
- `atm list` — bounded metadata rows without full bodies
- `atm ack <message-id> "<reply>"` — with automatic reply generation
- `atm clear` — removes clearable messages with `--idle-only` and age filters
- **Wait mode** — `atm read --timeout <seconds>` blocks until message arrives
- **Seen-state watermark** — per-inbox high-water mark; `--no-update-seen` preserves it

### Unique Features
- **Post-send hooks** — `.atm.toml` `[[atm.post_send_hooks]]` rules run after sends/acks with wildcard recipient matching; receive structured `ATM_POST_SEND` JSON
- **Tmux auto-nudge** — README includes tmux hook script for keystroke injection
- **File reference policy** — forbidden files copied to team share with rewritten references
- **Idle notification deduplication** — replaces older unread idle notifications from same sender
- **Message successor chains** — `add-details` (composes predecessor) and `supersede` (replaces prior)
- **Ephemeral messages** — time-bounded with `expires_at`, periodic sweep cleanup
- **Missing-config fallback** — delivers to existing inboxes even when `config.json` missing
- **Multi-workspace same-host** — concurrent `ATM_HOME` workspaces sharing one daemon/SQLite DB

### Diagnostics & Tooling
- `atm doctor` — config, identity, mailbox readiness, observability health, roster drift
- Structured logging: `atm log snapshot`, `atm log filter`, `atm log tail`
- Agent spawning via Claude Code `Task` API

---

## 2. Weave Overlap

| ATM Feature | Weave Equivalent | Notes |
|-------------|------------------|-------|
| `send` / `ack` | `weave send` / `weave ack` | Parity |
| `read` / `list` | `weave inbox` / `weave inbox --json` | Parity |
| `clear` | `weave gc` | Parity |
| Cross-team messaging | `weave send --to-store` (Tier-2) | Partial |
| Team backup/restore | — | **Gap**: no backup/restore |
| Post-send hooks | — | **Gap**: no send hooks |
| Message successor chains | `weave reply` | Partial (no supersede) |
| Ephemeral messages | — | **Gap**: no TTL on messages |
| Idle deduplication | — | **Gap**: no dedup |
| File reference policy | — | **Gap**: no file attachment policy |
| Seen-state watermark | `weave inbox --since` | Partial |
| Wait mode | `weave watch` | Partial (watch polls, wait blocks) |

---

## 3. Weave Gaps

### Medium Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **Message backup / restore** | Disaster recovery for mailbox state |
| 2 | **Post-send hooks** | Automation triggered by messaging events |
| 3 | **Message supersede / successor chains** | Replace outdated messages with newer context |
| 4 | **Ephemeral messages** | Auto-expire temporary coordination messages |
| 5 | **Idle notification deduplication** | Prevent spam from repeated idle pings |

### Low Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 6 | **File reference policy** | Attach files to messages with access controls |
| 7 | **Blocking wait mode** | `weave read --timeout` blocks instead of polling |

---

## 4. Proposed WL Items

- `WL-035` — Mailbox backup / restore (ZIP snapshot of SQLite + config)
- `WL-036` — Post-send hooks (trigger external commands on send/ack)
- `WL-037` — Message supersede / successor chains (replace prior messages)
- `WL-038` — Ephemeral messages with TTL and auto-sweep
- `WL-039` — Idle notification deduplication

---

## 5. Integration Opportunities

- ATM is tightly coupled to Claude Code team directories; weave is runtime-agnostic
- Both are Rust + SQLite; data models are compatible
- weave's injector backends could power ATM's tmux nudge (and extend it to zellij/kitty)

---

## 6. Notes

- ATM's daemon-free design aligns with weave's philosophy
- The post-send hook pattern is powerful and low-cost to add
- Ephemeral messages + deduplication are quality-of-life improvements for busy meshes
