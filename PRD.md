# weave — Product Requirements Document

**Status:** v0.1.0 (working MVP)
**Owner:** drdave
**One-liner:** A Rust-native agent-to-agent session mesh with a native injector — let coding-agent sessions (Claude Code, etc.) message each other and push *into* a running session's terminal pane, no Python and no external daemon required.

---

## 1. Problem

Claude Code sessions are isolated: no built-in way for one session to message another. Two prior attempts on this box:

1. **`mcp-broker`** (Python + libSQL) — works, but **poll-only**: the recipient only sees a message when it calls `broker_inbox`. No push; a running session never gets flagged.
2. **`repowire`** (Python) — does push via a daemon + peer registry + **tmux** pane injection. But: it is **tmux-first with no native zellij injector**, it is Python (a runtime + venv to manage), and it is a large product surface (relay, telegram, dashboard) for what is, locally, a small job.

This box's daily shell is **zellij** (via yazelix). Neither prior tool gives clean, native, zellij-aware push in a single dependency-free binary.

## 2. Vision

**weave** is a single static Rust binary that:

- Gives every agent session a **name** and a persistent **mailbox**.
- **Pushes** new messages into a running session's pane via a **native injector** that speaks both **tmux** and **zellij** (extensible to kitty/wezterm/screen), with no Python and no reliance on repowire.
- Falls back to **hook-driven delivery-on-next-turn** when no multiplexer is present, so it degrades gracefully everywhere.
- Is **MCP-native**: exposes `weave_*` tools over stdio so any agent can send/read without shelling out.
- Stores state in a **libSQL-compatible SQLite** file (no daemon; the DB is the broker).

## 3. Goals / Non-goals

### Goals
- G1. One self-contained binary; no Python, no runtime venv.
- G2. **Native injector** for tmux + zellij; pluggable trait for more muxes.
- G3. MCP stdio server with the proven broker semantics (per-reader read tracking, broadcast, history, sessions, clear-with-confirm).
- G4. Claude Code lifecycle hooks: auto-register on `SessionStart`, auto-deliver on `UserPromptSubmit`/`Stop`.
- G5. Graceful degradation: no mux → next-turn delivery; mux missing → clear message, never crash.
- G6. libSQL-compatible on-disk format (clean path to Turso sync/replicas later).

### Non-goals (for now)
- Cross-machine relay, Telegram/Slack bridges, web dashboard (repowire's territory; revisit if needed).
- A long-running daemon. The store + mux CLIs are enough for local push; a presence daemon is optional future work.
- Being a task scheduler / kanban / merge gate.

## 4. Architecture

### Current (v0.1 MVP) — single binary crate `weave`
```
src/
├── model.rs   types (Message, Peer), broadcast set, UTC ts formatting
├── store.rs   SQLite store (rusqlite, bundled): messages/reads/peers; per-reader read tracking
├── inject.rs  native injector: Mux{Tmux,Zellij,None}, Target, detect_target(), commands_for(), inject()
├── mcp.rs     MCP stdio JSON-RPC 2.0 server; weave_* tools; injects nudge on send
└── main.rs    clap CLI: mcp | send | inbox | peers | register | inject | hook <event>
```
No daemon. Each session runs `weave` as: (a) an MCP server (per session, over stdio) and (b) lifecycle-hook invocations. All instances share `~/.local/share/weave/messages.db`.

### How push works (no daemon)
`tmux send-keys -t <pane>` reaches any pane on the tmux server; `zellij --session <name> action write-chars` reaches any zellij session. So the **sender** injects directly into the recipient's registered pane. The peer registry (a `peers` table) maps `name → (mux, pane/session id)`, captured from `$TMUX_PANE` / `$ZELLIJ_SESSION_NAME` at `SessionStart`.

### Target architecture (roadmap)
Split into a workspace: `weave-core` (store+model), `weave-inject` (mux adapters), `weave-mcp`, `weave` (CLI). Optional `weaved` presence daemon for online/offline + lifecycle eviction.

## 5. Data model
- `messages(id, ts, sender, recipient, subject, body)`
- `reads(message_id, reader, ts)` — per-reader read state; a broadcast is delivered once per reader.
- `peers(name, mux, target, cwd, last_seen)` — the injection registry.

## 6. Injector design
- `Mux` enum + `Target { mux, id }`.
- `detect_target()` reads env to learn the current pane.
- `commands_for(target, text)` is a **pure function** returning the exact argv(s) — unit-tested for tmux and zellij without a mux present.
- `inject()` checks the mux binary is on PATH, runs the commands, and surfaces a clear error if the pane/session is gone (caller falls back to next-turn delivery).
- Hardening (roadmap): bracketed-paste close via hex `ESC[201~` for tmux (repowire's lesson) so injection never triggers a TUI cancel mid-tool-call.

## 7. MCP tools
`weave_send` (injects on delivery), `weave_inbox`, `weave_history`, `weave_sessions`, `weave_clear` (scope=all needs confirm), `weave_peers`.

## 8. Lifecycle hooks (Claude Code)
- `SessionStart` → `weave hook session` → register peer (name+pane).
- `UserPromptSubmit` → `weave hook prompt` → drain unread to stdout → enters context (auto-delivery without a mux).
- `Stop` → `weave hook stop` → same drain.
- `SessionEnd` → (roadmap) deregister.

## 9. Milestones
- **M0 (done):** store, native injector (tmux+zellij), MCP server, CLI, hooks, tests, builds.
- **M1:** `weave setup` (auto-register MCP + hooks in `~/.claude`), wizard integration, bracketed-paste hardening, live injection validated on the zellij target.
- **M2:** workspace split; `Store` trait + libSQL (Turso crate) backend.
- **M3:** optional `weaved` presence daemon (online/offline, lifecycle eviction); more muxes (kitty, wezterm, screen).
- **M4 (maybe):** cross-machine + phone surfaces, if the need appears.

## 10. Comparison
| | mcp-broker | repowire | **weave** |
|---|---|---|---|
| Language | Python | Python | **Rust (1 binary)** |
| Push to running session | ❌ poll only | ✅ tmux | ✅ **tmux + zellij (native)** |
| zellij injector | n/a | ❌ | ✅ |
| Daemon required | no | yes | **no** (optional later) |
| MCP-native | ✅ | ✅ | ✅ |
| Cross-machine / telegram | ❌ | ✅ | ❌ (non-goal for now) |
