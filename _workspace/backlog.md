# weave-loop backlog

Seeded from `TASKS.md` M1/M3, reordered to surface the item with an existing planner plan
and the open gaps the user flagged.

## Active / high-priority
- [x] WL-001: Workspace split — carve `weave-core`, `weave-inject`, `weave-mcp`, `weave` bin (TASKS.md M3; ROADMAP-v0.2 Phase 1). Merged via PR #30 (sha 82ea6dd).
- [x] WL-002 Phase A: presence daemon store + CLI — merged via PR #32.
- [x] WL-002 Phase B: MCP daemon tools (`weave_daemon_start`/`stop`/`status`) — merged via PR #33.
- [x] WL-003: zellij pane targeting — capture `ZELLIJ_PANE_ID` at registration, pass `--pane-id` to `write-chars`/`write` so injection hits the correct pane instead of the focused one (TASKS.md M1). Merged via PR #50.
- [x] WL-004: Integration tests for daemon lifecycle — env-configurable heartbeat/evict intervals; idempotency + stale-pidfile coverage. Merged via PR #50.
- [x] WL-005: Harden / execute `ralph-weave.sh` unified loop in anger — fixed broken guardian default, added gh pre-flight, stale-report scrubbing, working-tree sanity check, WEAVE_SKIP_GUARDIAN escape hatch. Merged via PR #50.
- [x] WL-006: `weave setup` — auto-register MCP server + write Claude hooks, merging with existing hooks (TASKS.md M1). Implementation verified in `setup.rs`; merged via PR #50.
- [x] WL-007: Bracketed-paste hardening for tmux — close paste mode with hex `ESC[201~` instead of bare Enter (TASKS.md M1). Implementation verified in `weave-inject/src/inject.rs`; merged via PR #50.
- [x] WL-008: Validate live injection on the zellij target box (TASKS.md M1). **Validated:** live injection works; `connect` → Live, `inject`/`notify`/`send` → `injected/ok` delivery trace. **Bug found & fixed:** zellij `list-sessions` emits ANSI color codes by default, causing `id_present` token match to fail → liveness probe falsely reports absent. Fixed by adding `--no-formatting` to the liveness probe argv. Also verified `WEAVE_MUX_DIR` is required on Nix systems where zellij lives in `/nix/store/.../toolbin` (outside `trusted_dirs()`).
- [x] WL-009: Wizard integration — build `weave` in RTX-5090 image, run `weave setup` (TASKS.md M1). **Validated:** built weave 0.2.0 in release on the live RTX-5090 box (2x NVIDIA GeForce RTX 5090, 32GB each, CUDA 13.2, Threadripper PRO 7965WX, 498GB RAM). Ran `weave setup` — MCP registered (✓ Connected), hooks wired (session/prompt/stop/wake), settings.json updated. Live injection verified: `notify` → `transport_delivered` → `injected/ok` delivery trace.

## M1 — Make it real on the box
- [x] WL-010: Decide retirement of `mcp-broker` / `repowire` (TASKS.md M1). Decision recorded in ARCHITECTURE.md §8.

## M3 — Robustness & reach
- [x] WL-011: Optional `weaved` presence daemon — online/offline, lifecycle eviction (TASKS.md M3). **Duplicate:** fully implemented in WL-002 Phase A/B (heartbeat + evict + liveness + MCP tools). No additional work required.
- [x] WL-012: More mux adapters — kitty (`kitten @ send-text`), wezterm (`wezterm cli send-text`), GNU screen (`screen -X stuff`) (TASKS.md M3). **Duplicate:** fully implemented in inject.rs with detect_target, commands_for, liveness probes, id validation, and unit tests for each backend.
- [x] WL-013: Config file — `~/.config/weave/config.toml` with default identity, nudge template, mux preference (TASKS.md M3). `mux_preference` added to `Config`; `detect_target` honors it across CLI, hooks, and MCP.

## Gaps discovered from repowire cross-reference (2026-06-07)
- [x] WL-014: Reminder injection for open asks — unacked asks resurface as a content-free nudge at the start of every subsequent prompt on the recipient side (repowire parity).
- [x] WL-015: Structured question types — extend `ask` to support choice, free-text, and tool-permission envelopes; render as actionable prompts in the recipient's pane (repowire parity). Schema + Store trait + both backends + CLI + MCP call sites done. kind/options columns added; AskKind enum with FreeText/Choice/ToolPermission.
- [x] WL-016: Scheduler / cron for messages — one-shot and recurring scheduled deliveries (`@daily`, `@hourly`, etc.) with SQLite-backed persistence and drift-safe execution (repowire parity). Schema + Store trait + both backends + CLI + MCP + tick mechanism (prompt hook + explicit `weave tick`) + tests. 442 passed (sqlite), 412 passed + 1 ignored (libsql).
- [x] WL-017: Mesh memory system — filesystem-backed scoped memory under `~/.config/weave/memory/` (global, project, persona, orchestrator) with CLI read/write/search and automatic context prefixing on ask delivery (repowire parity). Core memory.rs module + CLI + MCP tools + context prefixing on ask/send/reply/answer. 463 passed (sqlite), 433 passed + 1 ignored (libsql).
- [x] WL-018: Birth certificates / runtime identity envelopes — mint unguessable nonces at `SessionStart` to prevent path-based identity takeover during lazy MCP registration (repowire parity). birth_cert column + getrandom nonce + cert verification on re-register + backward-compat + CLI --cert + WEAVE_BIRTH_CERT env + MCP tools. 463 passed (sqlite), 433 passed + 1 ignored (libsql).
- [x] WL-019: Co-orchestrator support — allow multiple live orchestrators to coexist in the same circle for resilience against rate limits or credit caps (repowire parity). Non-force claims additive, force claims still steal. orchestrator_status returns all live holders. 463 passed (sqlite), 433 passed + 1 ignored (libsql).
- [x] WL-020: GitHub review queue integration — track PR review state across peers (`review_queue`, `mark_reviewed(pr_url)`) via CLI and MCP tools (repowire parity).
- [x] WL-021: PreToolUse tool approval — gate mutating tools (Bash, Edit, Write) behind blocking approval questions from human surfaces/peers, denying by default on timeout (repowire parity).
- [x] WL-022: Streamable-HTTP MCP transport — localhost-only opt-in endpoint for remote agents, bearer-authenticated, dangerous tools disabled by default (ROADMAP-v0.3 §4; repowire parity).
- [x] WL-023: iTerm2 injector backend — native injection support for iTerm2 terminal multiplexer (ROADMAP-v0.3 §6). AppleScript via osascript; no liveness probe (fail-open).
- [x] WL-024: Reservation leases — lightweight advisory file locks between agents to coordinate exclusive access to shared resources (ROADMAP-v0.3 §5). `leases` table + `reserve_lease`/`release_lease`/`list_leases` Store methods, both backends, CLI `weave lease {reserve,release,list}`, 3 MCP tools, integration tests. 494 passed (sqlite), 462 passed + 1 ignored (libsql).
- [x] WL-025: Stop-boundary wake — blocking `Stop`/`SubagentStop` hook that returns `additionalContext` to drive the next turn without polling (ROADMAP-v0.3 §1). `weave hook stop --wake` drains inbox with mark_read=true and emits `{"decision":"block","reason":...}` JSON when unread messages exist; `WEAVE_STOP_WAKE=1` env var also enables it. Default `stop` behaviour remains peek-only.
- [x] WL-026: Idempotency keys & trace IDs — per-message idempotency keys and distributed trace IDs for end-to-end debugging across stores (ROADMAP-v0.3 §3). 507 passed (sqlite), 467 passed + 1 ignored (libsql).
- [x] WL-027: Broadcast notify / broadcast ask — fan-out notifications and asks to all online peers in the caller's circle, not just `--to all` store broadcast (repowire parity). `weave_broadcast_notify` + `weave_broadcast_ask` MCP tools + `weave broadcast-notify` + `weave broadcast-ask` CLI. 509 passed (sqlite), 469 passed + 1 ignored (libsql).
- [x] **FrankenNetworkX crate extraction** (bonus, not in original backlog) — `fnx-classes` + `fnx-algorithms` + `fnx-runtime` wired in via Cargo git dependencies. `weave graph` command builds peer/message communication graph and runs connected_components + degree_centrality + density. 510 passed (sqlite), 470 passed + 1 ignored (libsql).

## Gaps from mcp_agent_mail cross-reference (2026-06-07)
- [x] WL-028: FTS5 full-text search on messages, threads, and subjects (mcp_agent_mail parity). `Store::search` with FTS5 virtual table (sqlite) + LIKE fallback (libsql). `weave search` CLI + `weave_search` MCP tool. 512 passed (sqlite), 472 passed + 1 ignored (libsql).
- [x] WL-029: Advisory file leases with TTL expiry and conflict detection (mcp_agent_mail parity). `lease_path_normalize` + `lease_path_conflicts` for prefix-based path conflict detection. Same-holder re-reserve extends TTL. Auto-sweep before list/reserve. `weave lease sweep` CLI + `weave_lease_sweep` MCP. 517 passed (sqlite), 477 passed + 1 ignored (libsql).
- [x] WL-030: Pre-commit Git hook for file reservation guard (mcp_agent_mail parity). `weave lease guard` checks staged files against active leases. `weave setup --git-hooks` installs idempotent pre-commit hook. 519 passed (sqlite), 479 passed + 1 ignored (libsql).
- [x] WL-031: Message importance / priority levels with urgent filtering (mcp_agent_mail parity). `MessagePriority` enum; `--priority` on `weave send`/`notify`/`broadcast-notify`; priority field on MCP `weave_send`/`weave_notify`/`weave_broadcast_notify`; cross-store priority carried through `Intent`/`outbox` and applied on pull; `weave_set_message_priority` MCP tool. 528 passed (sqlite), 488 passed + 1 ignored (libsql).
- [x] WL-032: Per-peer contact policies (open / auto / contacts_only / block_all) with explicit request/respond handshake (mcp_agent_mail parity). `ContactPolicy` enum; `weave peer-policy` CLI; `weave_set_peer_policy` / `weave_get_peer_policy` MCP tools. 528 passed (sqlite), 488 passed + 1 ignored (libsql).
- [x] WL-033: Thread summarization via LLM integration (mcp_agent_mail parity).
- [ ] WL-034: Static mailbox export — self-contained portable HTML bundle with search (mcp_agent_mail parity).

## Gaps from atm-core cross-reference (2026-06-07)
- [ ] WL-035: Mailbox backup / restore — ZIP snapshot of SQLite + config + hooks (atm-core parity).
- [ ] WL-036: Post-send hooks — trigger external commands on send/ack with wildcard recipient matching (atm-core parity).
- [ ] WL-037: Message supersede / successor chains — replace prior messages with updated context (atm-core parity).
- [ ] WL-038: Ephemeral messages with TTL and auto-sweep (atm-core parity).
- [ ] WL-039: Idle notification deduplication — replace older unread idle pings from same sender (atm-core parity).

## Gaps from cross_agent_session_resumer cross-reference (2026-06-07)
- [ ] WL-040: Session export/import in canonical format (JSON/IR) for portability across weave instances (casr parity).
- [ ] WL-041: Read-back verification for destructive operations — verify config/hook rewrites before declaring success (casr parity).
- [ ] WL-042: Multi-provider lifecycle hook templates — Codex CLI, Gemini CLI, Aider hook scaffolding (casr parity).

## Gaps from claude-code-router / cc-mirror cross-reference (2026-06-07)
- No direct gaps identified — these are model-routing / launcher tools orthogonal to weave's messaging mesh scope. Potential integration note: `weave setup --variant` could support per-variant MCP registration if cc-mirror is detected.
