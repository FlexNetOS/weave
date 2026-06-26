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
| repowire features that weave **HAS or SUPERSETS** | **36 of 36 core dashboard/messaging/security/transport categories** |
| **Genuine repowire dashboard gaps** | **0** — the browser dashboard is covered Rust-natively by WL-083 |
| **Superseded by design** (no-daemon) | **1** — API-key relay isolation → per-source token/owner-only-writes |
| repowire agent-runtimes not yet wired | **3** — Antigravity, OpenCode, Pi (weave wires Claude/Codex/Gemini/Aider) |
| weave capabilities repowire **does NOT have** | **14** (§9) |

**Conclusion: weave covers the real repowire dashboard and remains a Rust-native
superset, with real new capability.** **agent spawn/kill DID ship** (WL-047),
**the human surfaces DID ship** (WL-048/WL-083 — Rust-native dashboard +
Telegram/Slack behind `--features surfaces`), **governed web reach DID ship**
(WL-049 — stealth browsing via obscura behind `--features obscura`), **enforcing
PreToolUse DID ship** (WL-055), and **daemon-free cross-machine push DID ship**
(WL-056). weave additionally ships **14** capabilities repowire never had (signing,
summarization, leases, FTS, federation, dual backend, graph analytics, …), and
**EXCEEDS** repowire's hosted-relay web reach with *governed stealth browsing*
(WL-049 / ADR-0002): web access **without a daemon**, gated + leased + audited +
SSRF-guarded, where repowire offered only an ungoverned hosted relay.

The remaining tracked items are not dashboard blockers: **3 additional
agent-runtimes are not yet wired** (Antigravity, OpenCode, Pi — weave wires
Claude/Codex/Gemini/Aider) and **2 minor conveniences** are missing
(agents-folder scaffold, the `SOUL.md` persona-file convention).

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
| Structured tool-approval questions | `weave_ask_permission` + `weave_permission_*` | ✅ HAVE | WL-021 + **WL-055** (enforcing PreToolUse hook, opt-in via `weave setup --pretooluse`) |
| Message supersede / successor chains (atm-core) | `weave send --supersedes <id>` / `weave_send {supersedes}` | ✅ HAVE | **WL-037**; additive `messages.superseded_by` (both backends); hide-from-unread, flag-in-history; sender-only authz |
| Idle notification dedup (atm-core) | `weave notify --dedup-idle` / `weave_notify {dedupIdle}` | ✅ HAVE | **WL-039**; reuses `messages.superseded_by`; eligible only on `kind='idle'` rows + same (sender,recipient) + unread; sender-scoped; additive nullable `messages.kind` (both backends); never dedups a real message or another sender's pings |
| Ephemeral messages / TTL auto-sweep (atm-core) | `weave send --ttl <secs>` / `weave_send {ttl}` | ✅ HAVE | **WL-038**; additive nullable `messages.expires_at` (both backends); delete-on-sweep via `gc()` fold-in + opportunistic `sweep_expired_messages`; TTL-capped (`MAX_MSG_TTL_SECS = 86400`); cross-store via `outbox.ttl` |
| Session export / resume (cross_agent_session_resumer / casr) | `weave session export` / `weave session import` | ✅ HAVE | **WL-040 + WL-040b**; portable, schema-versioned JSON interchange (messages + mesh memory + **ask threads + ask-many groups**) across distinct instances; messages reuse `Store::send` (free id-remap, idempotent dedup, synth key for keyless legacy); **ask-thread fidelity now complete (WL-040b)** — asks replayed faithfully via the dual-backend `Store::import_ask` (materialized directly in their exported `AskState`, message links remapped to the freshly minted local ids, dangling refs skipped+counted) and groups via `Store::import_ask_group` (replayed before children, `parent_id` rewired); only the `reply_to` chain pointer is intentionally dropped (regenerated source id); identity remap via `--as`; `--dry-run`; idempotent re-import; additive envelope (no `schema_version` bump); **no new standing MCP tool** (CLI-only). Peers excluded by design. See `FORMAT-session-export.md` |
| Verify-before-success on destructive config/hook rewrites (casr "verify before declaring success") | `weave setup` / `weave uninstall` / `weave setup --git-hooks` / `weave restore` read-back-verify | ✅ HAVE | **WL-041**; after the atomic write, re-open + re-parse the file and assert weave's intended entries are present (merge) / absent (prune) AND every pre-existing foreign hook survived; git hook asserts guard line + shebang + foreign-content preservation (append-only); restore asserts restored config/settings bytes == archived payload + settings.json re-parses as JSON object; descriptive `Err` (names `.bak` recovery) on mismatch. Mirrors the WL-035 backup read-back; CLI-only (no new MCP/standing tool) |
| Multi-provider host wiring (casr — wire any host, not just Claude) | `weave setup --provider <claude\|codex\|gemini\|aider>` / `weave uninstall --provider <…>` | ◐ PARTIAL | **WL-042**; generalizes setup from Claude-only to four hosts, each written Rust-natively into its own config (claude→`~/.claude/settings.json`, codex→`~/.codex/config.toml` `notify` argv, gemini→`~/.gemini/settings.json` hooks, aider→`~/.aider.conf.yml` stanza) with the SAME idempotent + never-clobber-foreign + atomic + read-back-verified (WL-041) discipline. Default `claude` byte-for-byte unchanged (regression-tested). **No new dependency** (line-based TOML / hand-templated YAML — no `toml`/`serde_yaml` added). PARTIAL because gemini's hook key is **unconfirmed** and aider's hook surface is **limited** — both scaffold-with-caveat (printed each run + tracked in `MULTI-SURFACE-PARITY.md`). CLI-only (no new MCP/standing tool) |

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
| `job_create` / `run` / `retry` / `cancel` (continuity modes) | `weave_job_create/delegate/claim/update/result/show/list/cancel/status` + CLI `weave job dispatch` | 🟢 SUPERSET | durable `jobs` table; MCP/CLI delegation creates an assigned queued job and worker `JOB_DELEGATED` nudge; dispatch claims and runs a trusted external runner while Weave records progress/result |
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
| Browser dashboard (Next.js) | Rust-native dashboard (`weave dashboard`) plus repowire-compatible read/write endpoints | ✅ **HAVE** | **WL-048/WL-083** — Weave deliberately rejected the Next.js/Node runtime in favor of ADR-0004's Rust-native HTML+SSE surface. Current dashboard parity covers the real upstream multi-pane shape: peer roster, typed mesh feed, selected peer detail/transcript/reply, pending structured questions, notify/ask/answer/reply forms, selected job detail, cancel/recreate, spawn/kill Danger zone with allowlist posture, token-free Settings panel, `/api/snapshot`, `/peers`, `/api/events`, `/events?since=...`, `/jobs?view=summary`, `/jobs/{id}/status`, `/jobs/{id}/result`, `/asks/pending`, `/settings`, `/health`. Writes are bearer-gated and require `weave dashboard --write`; form adapters route through the same JSON-RPC `dispatch_request` path as MCP/CLI. |
| Telegram bot | Telegram bridge (`weave telegram`), poll-only relay + commands | ✅ **HAVE** | **WL-048/WL-073** — `weave/src/telegram.rs`, `/inbox`/`/peers`/`/sessions` plus gated `/send`/`/ask`/`/answer`/`/reply` through shared dispatcher |
| Slack bot | Slack bridge (`weave slack`), poll-only relay + commands | ✅ **HAVE** | **WL-048/WL-073** — `weave/src/slack.rs`, same shared command grammar as Telegram, `--features surfaces` |

## 7. Security

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| Bearer token auth (HTTP/WS/hooks) | bearer-auth HTTP MCP surface | ✅ HAVE | WL-022; `http.rs` |
| Spawn allowlists (`daemon.spawn.allowed_paths`) | `spawn_allowed_dirs` / `WEAVE_SPAWN_DIRS` + `trusted_dirs()` (two-layer gate) | ✅ HAVE | **WL-047**; cwd allowlist (deny-by-default, MCP hard-deny) + trusted child `argv[0]` |
| PreToolUse tool approval | `weave_ask_permission` / `weave_permission_*` **+ an enforcing PreToolUse hook** (`weave hook pretooluse`) | ✅ HAVE (enforcing, opt-in via `weave setup --pretooluse`) | WL-021 (primitive) + **WL-055** — the CLI drain raises a blocking approval on the existing ToolPermission machinery; **deny-by-default** (no approver / deny / its own short timeout ⇒ `permissionDecision:"deny"`), fail-CLOSED with weave's own short timeout (never relies on Claude's fail-open 600s). Claude-only, matcher `Bash|Edit|Write`; default OFF so it never surprise-blocks. `main.rs::handle_pretooluse_hook`, `setup.rs::merge_pretooluse_hook_at`, config `pretooluse_approver`/`pretooluse_timeout_secs`. No new standing MCP tool. |
| Post-send hooks (atm-core `[[post_send_hook]]`) | config `[[post_send_hook]]` (CLI/MCP send/notify/ack) | ✅ HAVE | **WL-036**; argv-only/no-shell, `argv[0]` trusted-dir, message fields env-only (body never exported), bounded/fault-isolated; no new standing tool |
| CORS restricted to localhost | localhost-only HTTP surface | ✅ HAVE | WL-022 |
| **No E2E encryption on relay (acknowledged gap)** | no relay at all; ed25519 signed identity + owner-only cross-store pull | 🟢 SUPERSET | `sign.rs`; closes repowire's own acknowledged weakness |

## 8. Transport & federation

| repowire | weave equivalent | Verdict | Evidence |
|---|---|---|---|
| stdio MCP server | `weave mcp` (stdio) | ✅ HAVE | `mcp.rs` |
| Streamable HTTP MCP (localhost, bearer) | HTTP MCP surface | ✅ HAVE | WL-022; `http.rs` |
| Hosted relay (`repowire.io`, outbound WSS) | consent-based cross-machine **PUSH** (ADR-0005) + cross-store federation (Tier-1) + Tier-2 pull | ✅ HAVE (daemon-free push, owner-only-writes) | WL-056: `weave push --to <name> --host <url:port>` POSTs a signed Intent to B's bearer-gated `weave dashboard --write` endpoint; B commits into its OWN inbox via the SAME `commit_pulled` pipeline and lights its OWN pane WITHOUT polling. Daemon-free (no relay process — receive exists only while B opts into `serve`/`dashboard --write`), owner-only-writes (B commits its own row), verify-on-commit (signature gates identity), default build byte-identical. `mcp::tool_push`, `http.rs` `--bind` fail-closed, `main.rs` `Cmd::Push` |
| Self-hosted relay | cross-machine PUSH (ADR-0005) + Tier-2 pull from remote sources | ✅ HAVE (daemon-free push) | WL-056 push (A→B, B idle) + Tier-2 pull; no hosted relay process required |
| API-key scoped relay isolation | per-source token/timeout parity on federated pull | 🧭 SUPERSEDED | ARCHITECTURE §10 |

---

## 9. weave capabilities repowire does NOT have (the "more")

These have **no repowire equivalent** — they are why weave is a *superset*, not a port:

1. **ed25519 signed sender identity** — sign/verify, multi-key rotation, revocation audit log (`sign.rs`, `weave_doctor` verify-summary).
2. **LLM thread summarization** — cached in-store summaries (`llm.rs`, `summaries` table; WL-033).
3. **Advisory leases** — path leases with TTL expiry, prefix conflict detection, auto-sweep, pre-commit guard (`leases` table; WL-024/029/030).
4. **FTS5 full-text search** over messages/threads/subjects (libSQL backend also uses fts5 MATCH; there is no LIKE-only fallback path) (`weave_search`; WL-028).
5. **Dual storage backend** — bundled sqlite **and** libSQL/Turso behind one `Store` trait (repowire is SQLite-only).
6. **Cross-store federation (Tier-1)** + **Tier-2 cross-machine pull** with idempotency keys + trace IDs (WL-026) — relay-free remote reach.
7. **Message priority** (low/normal/high/urgent) + **per-peer contact policies** (open/auto/contacts_only/block_all) (WL-031/032).
8. **5-mux native + iTerm2 injection, paste-safe** (tmux/zellij/kitty/wezterm/screen) vs repowire's tmux-only — with the bracketed-paste fix repowire lacked (WL-007/012/023).
9. **Communication-graph analytics** — `weave graph` (connected components, degree centrality, density) via the FrankenNetworkX extraction.
10. **Stop-boundary wake** — blocking `Stop`/`SubagentStop` hook returns `additionalContext` to drive the next turn without polling (WL-025).
11. **GitHub review queue** across peers (`weave_review_*`; WL-020).
12. **`weave_doctor`** federation-health + signature verify rollup.
13. **One dependency-light static Rust binary, Python-free, no daemon** — the whole thing, vs repowire's Python runtime + daemon + Next.js stack.
14. **Portable mailbox backup/restore** (atm-core parity) — `weave backup`/`weave restore`, a dependency-free uncompressed-USTAR snapshot of the DB (`VACUUM INTO`, never a raw live copy) + config + Claude settings, read-back-verified, traversal-guarded (`archive.rs`; **WL-035**).

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
| ~~Rust-native human surfaces (dashboard/Telegram/Slack)~~ | **WL-048 / WL-052 / WL-083** ✅ done | Dashboard is Rust-native HTML+SSE (no Next.js/Python) with repowire-compatible JSON/action endpoints; Telegram/Slack bridges live behind `--features surfaces`. |
| `agents create` folder scaffolding | *(candidate WL)* | minor convenience; `weave config init` already scaffolds config |
| `SOUL.md` persona-file precedence | *(candidate WL)* | persona memory **scope** already exists; only the file convention is missing |
| ~~Cross-machine **PUSH** delivery~~ | **WL-056** ✅ done | ADR-0005 (accepted) — consent-based push (A→B, B idle) over the bearer-gated `dashboard --write` POST /api receive seam: `tool_push` commits via the SAME `commit_pulled` pipeline (owner-only-writes, verify-on-commit), `weave push` CLI sender, `--bind` fail-closed; no default daemon, default build byte-identical (§8, §10) |
| Wire repowire's remaining agent-runtimes | *(candidate WL)* | Antigravity, OpenCode, Pi (weave currently wires Claude/Codex/Gemini/Aider) |
| ~~Governed web reach (beyond repowire)~~ | **WL-049** ✅ done | ADR-0002 (accepted) — obscura-as-capability via spawn-and-speak MCP client, `weave_web`/`weave web`, deny-by-default + SSRF-guarded, no V8/tokio in core (§7, §9) |
| Token-light surface (engineering, not parity) | **WL-050..052** | ADR-0003 — progressive disclosure keeps the 70-tool superset token-light |
| ~~Enforcing PreToolUse approval gate~~ | **WL-055** ✅ done | the approval *primitive* (WL-021) now has a real PreToolUse hook that BLOCKS dangerous tools: `weave hook pretooluse` drain (deny-by-default, fail-closed, own short timeout) + opt-in `weave setup --pretooluse` wiring (matcher `Bash\|Edit\|Write`, never-clobber-foreign, read-back-verified). No new standing tool (§7) |

**Net:** with agent spawn/kill (WL-047), human dashboard/Telegram/Slack surfaces
(WL-048/WL-052/WL-083), governed web reach (WL-049), enforcing PreToolUse
(WL-055), and daemon-free push (WL-056) shipped, weave now covers the real
repowire dashboard and remains a Rust-native superset. Remaining items in this
matrix are minor convenience/runtime extensions (`agents create`, `SOUL.md`, and
extra provider runtimes), not blockers for the browser-dashboard port. The audit
confirms the docs' claim (PRD §8, ARCHITECTURE §0): **more than repowire, not less.**
