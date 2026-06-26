# Local repowire source parity audit for Weave

Date: 2026-06-26

## Scope and source of truth

This audit uses the local upstream source archive as the authority:

- zip: `/home/drdave/Desktop/meta/meta-yard/repowire-main.zip`
- extracted scratch copy: `.handoff/run/repowire-source-audit/repowire/repowire-main/` (gitignored)
- Weave baseline: `develop` after PR #154 (`79fbd3e`), audited on branch `codex/repowire-local-source-audit`

The audit is semantic, not a raw file diff. Weave intentionally does **not** adopt repowire's Python/FastAPI daemon or Next.js/Node dashboard runtime. The parity question is whether the user-visible and operator-visible behavior has an equivalent Rust-native, dependency-light Weave surface, or is explicitly superseded/non-goal.

## Source maps

### repowire dashboard and web surfaces

| Area | Upstream path/symbol | Behavior observed in local zip |
|---|---|---|
| Dashboard shell | `web/app/dashboard/page.tsx` | Next.js client shell with peer roster, selected-peer panel, mesh/jobs main switch, pending questions, top bar, settings dialog, spawn dialog, and SSE event wiring. Fetches `/peers`, `/events`, `/events/stream`, `/circles/{circle}/orchestrator`. |
| Peer roster | `web/app/dashboard/components/PeerRoster.tsx`, `types.ts::Peer` | Circle-grouped peer list with status, turn state, backend/model, branch/path/description filtering. |
| Selected peer | `web/app/dashboard/components/PeerView.tsx` | Chat/history/MCP tabs; timeline search; transcript pagination; notify/ask/answer/reply; attachment upload; session resume/notify controls; protected-thread controls. |
| Mesh feed | `web/app/dashboard/components/MeshFeed.tsx`, `types.ts::Event` | Typed feed of query/response/notification/broadcast/status/chat/ask/ack/peer-reaped events with attachment chips. |
| Event recovery | `web/app/dashboard/lib/useEventStream.ts` | `EventSource(${apiBase}/events/stream)` with backoff and gap fetch from `/events?since=<last_seen_id>`. |
| Pending questions | `web/app/dashboard/components/PendingQuestions.tsx`, `types.ts::AskQuestion` | Structured pending asks: acknowledgement/choice/text, answer POSTs to `/answer`. |
| Jobs | `web/app/dashboard/components/JobsView.tsx`, `types.ts::JobStatus` | `GET /jobs?view=summary`, per-job status, cancel, retry, completed/result-oriented detail. |
| Spawn/settings dialogs | `web/app/dashboard/components/DashboardDialogs.tsx` | `GET /spawn/config`, `POST /spawn`, `GET /panes/orphans`, `GET /health`; settings health/service-peer posture. |
| Marketing/docs static web | `web/app/page.tsx`, `web/components/marketing/*`, `web/public/*` | Product marketing, hosted assets, screenshots. Not part of Weave runtime parity. |

### repowire daemon/API surfaces

| Area | Upstream path/symbol | Behavior observed in local zip |
|---|---|---|
| Daemon app | `repowire/daemon/app.py` | FastAPI app wiring routes, state DB, event log, registry, router, scheduler, work store, relay client, job runner, static dashboard. |
| Messages | `repowire/daemon/routes/messages.py` | `/query`, `/notify`, `/broadcast`, `/session/update`, `/response`, `/events/chat`, `/events/chat_delta`, `/events`, `/events/stream`. |
| Asks/questions | `repowire/daemon/routes/asks.py` | `/ask`, `/ask-many`, `/questions/ask-blocking`, `/answer`, `/ack`, `/asks/pending`, wait/pickup/reminder and queued deliveries. |
| Peers/session registry | `repowire/daemon/routes/peers.py` | `/peers`, peer detail/doctor/identity, orphan pane link, role/circle/orchestrator, timeline/search/transcript, peer MCP server CRUD. |
| Jobs/work | `repowire/daemon/routes/work.py` | `/work` and `/jobs` create/list/status/patch/run/retry/result/cancel. |
| Spawn/control | `repowire/daemon/routes/spawn.py`, `repowire/spawn.py` | `/spawn/config`, `/spawn`, `/kill-peer`, restart, rehook, switch backend, destructive ownership checks. |
| Sessions | `repowire/daemon/routes/sessions.py` | `/sessions/{id}/controls/notify` and `/sessions/{id}/controls/resume`. |
| Schedules | `repowire/daemon/routes/schedules.py` | One-shot/recurring schedule create/list/delete. |
| Attachments | `repowire/daemon/routes/attachments.py` | Upload/download attachment blobs under `~/.repowire/attachments` with size/cap checks. |
| Traces/reviews | `repowire/daemon/routes/traces.py`, `reviews.py` | Delivery trace lookup and review queue CRUD. |
| Relay/share | `repowire/relay/server.py`, `routes/shares.py`, `relay/*` | Hosted relay auth, daemon registration, `/events/stream` proxy, share tokens, `/s/{share_id}` viewer/ask, WebSocket relay. |
| Hooks/runtime | `repowire/hooks/*` | Session/prompt/stop/pretooluse/chat delta/tmux/websocket hook handlers and lifecycle handling. |
| Provider installers | `repowire/installers/*`, `agent_backends.py`, `agent_types.py` | Claude Code/Codex/Gemini/OpenCode/Pi/Antigravity setup and post-spawn wiring. |
| Config/state/schema | `repowire/config/*`, `repowire/daemon/state/*` | YAML config, spawn config, relay config, sqlite/json state stores for events/work/schedules/session bindings/queues. |
| Orchestrator/persona/memory | `repowire/orchestrator/*`, `repowire/memory.py` | Persona templates, SOUL, memory path/read/search/write behavior. |
| MCP/HTTP MCP | `repowire/mcp/server.py`, `repowire/peer_mcp.py`, `repowire/channel/server.ts` | MCP tools, streamable HTTP MCP mount at `/mcp`, peer backend MCP config management, TypeScript channel server. |
| Telegram/Slack | `repowire/telegram/bot.py`, `repowire/slack/bot.py` | Bot bridges for chat/control. |

### Weave current map

| Area | Weave path/symbol | Behavior/proof surface |
|---|---|---|
| Rust dashboard shell | `weave-mcp/src/dashboard.rs::render_dashboard`; `weave/src/main.rs::Cmd::Dashboard`; `weave-mcp/src/http.rs::serve_dashboard` | Server-rendered Rust HTML+SSE. Sections: Peer roster, Selected peer, Pending questions, Selected job, Actions, Danger zone, Settings, Mesh feed, Control plane. |
| Browser read APIs | `weave-mcp/src/dashboard.rs::route`; `weave-mcp/src/http.rs::handle_dashboard_connection` | `GET /api/snapshot`, `/peers`, `/api/events`, `/events?since=...`, `/events/stream`, `/jobs?view=summary`, `/jobs/{id}/status`, `/jobs/{id}/result`, `/asks/pending`, `/settings`, `/api/settings`, `/health`, `/peers/{name}/transcript`. |
| Browser write APIs | `weave-mcp/src/http.rs::dashboard_action_tool`, `dashboard_action_request`, `dispatch_request` | Form adapters for notify/ask/answer/reply/job cancel/job recreate/session controls/spawn/kill. Writes require `weave dashboard --write` and bearer/cookie auth, then route through canonical MCP dispatcher. |
| MCP tools | `weave-mcp/src/mcp.rs` | Single MCP server/tool dispatcher with message/ask/job/schedule/memory/spawn/kill/session/export/push/web surfaces and dangerous-tool gating. |
| CLI | `weave/src/main.rs::Cmd` | Native CLI for setup/uninstall/provider-switch, send/notify/broadcast/pull/push/reply, ask/answer/ask-many, jobs/schedules/leases, sessions export/import, dashboard/server, telegram/slack, export/backup/restore, spawn/kill, hooks/pretooluse. |
| Store/schema | `weave-core/src/store.rs`, `store_libsql.rs`, `model.rs` | Durable messages, asks, ask-many groups, sessions, schedules, jobs, leases, memory, outbox/federation, dual backend. |
| Config/security | `weave-core/src/config.rs`, `webpolicy.rs`, `sign.rs`; `weave-inject/src/inject.rs` | Rust config, no-shell argv-only injection/spawn, trusted program resolution, spawn allowlist, bearer tokens, signatures, web policy. |
| Runtime bridges | `weave/src/telegram.rs`, `weave/src/slack.rs`, `weave/src/setup.rs`, `weave/src/provider_switch.rs` | Telegram/Slack poll bridges; multi-provider host setup for Claude/Codex/Gemini/Aider; provider-switch helpers. |
| Runtime proof/docs | `weave/tests/integration.rs`, `docs/REPOWIRE-PARITY.md`, `.handoff/loop/plan_WL-083_repowire_web_ui.md`, `.handoff/run/repowire-final-smoke/*` | Integration/runtime evidence from prior dashboard slices plus current verification below. |

## Required audit matrix

Verdicts:

- **covered**: Weave has equivalent behavior.
- **superset**: Weave has equivalent behavior plus stronger/additional native behavior.
- **superseded**: Weave intentionally solves the requirement differently because of no-daemon/Rust-native design.
- **gap**: true missing behavior that should be implemented.
- **unclear**: needs owner/product decision or live external validation.

| repowire path/symbol/component | Upstream behavior | Weave path/symbol/route/tool | Proof type | Verdict | Notes/action |
|---|---|---|---|---|---|
| `web/app/dashboard/page.tsx` | Multi-pane dashboard shell | `weave-mcp/src/dashboard.rs::render_dashboard` | code/test/runtime/doc | covered | Rust-native shell includes the required peer roster, selected peer, pending asks, jobs, actions, danger, settings, mesh feed, and control plane sections. |
| `PeerRoster.tsx`, `types.ts::Peer` | Peer roster grouped/filterable with status/turn/path/backend metadata | `render_peer_cards`, `/peers`, `DashboardSnapshot.peers` | code/test/doc | covered | Weave roster is not a React clone, but includes peer count/live state, identity/session/path metadata and JSON roster endpoint. |
| `PeerView.tsx` selected peer summary | Selected peer view with session, cwd/path, role, branch/metadata, description | `render_selected_peer_detail`, `/peers/{name}/transcript` | code/test/doc | covered | Weave renders selected peer detail and transcript preview. |
| `PeerView.tsx` timeline search | Search and page a peer timeline/transcript | `/peers/{name}/transcript?q=...&before=...`; store search/history | code/test/doc | covered | Endpoint name differs from repowire `/timeline/search`, but behavior is present via transcript query and pagination. |
| `PeerView.tsx` reply/notify/ask/answer | Compose and control messages from dashboard | `/api/notify`, `/api/ask`, `/api/answer`, `/api/reply` form adapters to `dispatch_request` | code/test/doc | covered | Browser writes inherit canonical MCP/CLI validation and nudge/injection behavior. |
| `PeerView.tsx` session controls | Notify/resume active session | Dashboard session control forms to canonical session/notify tools | code/test/doc | covered | Rust-native session controls are explicit and bearer/write gated. |
| `PeerView.tsx` protected-thread/drafts/templates | Client-side React affordances for freezing/drafts/templates | no direct Weave equivalent | code | superseded | Weave's dashboard is stateless server-rendered HTML; no React client state is intentionally carried over. Not a core parity gap. |
| `PeerView.tsx` attachments | Upload/download attachments for messages | none in dashboard/API | code | gap | Low priority for dashboard parity because required audit areas did not include attachments; keep as a tracked non-dashboard/web affordance gap if binary attachments become a requirement. |
| `PeerView.tsx` peer MCP tab and `/peers/{name}/mcp` | Inspect/mutate per-peer MCP server config | `weave mcp` server + setup/provider config; no peer-scoped dashboard CRUD | code/doc | superseded | Weave treats MCP as its own native server/config seam, not daemon-managed per-peer MCP CRUD. Add only if owner wants browser-side provider config mutation. |
| `MeshFeed.tsx`, `types.ts::Event` | Typed mesh feed | `render_feed_cards`, `/api/events`, `typed_events_json` | code/test/runtime/doc | covered | Weave exposes typed message/ask/job/peer event feed. |
| `useEventStream.ts` | SSE on `/events/stream` with gap recovery via `/events?since=` | `DASHBOARD_SCRIPT`, `Route::Events`, `Route::EventsJson` | code/test/runtime/doc | covered | Current page opens `/events/stream` and fetches `/events?since=...`; route tests cover both. |
| `PendingQuestions.tsx` | Structured question answers | `render_pending_questions`, `/asks/pending`, `/api/answer`, `AskKind::{Choice,ToolPermission,FreeText}` | code/test/doc | covered | Choice buttons, tool approve/deny, and free-text forms are rendered. |
| `JobsView.tsx` | Jobs list/detail/status/result/cancel/retry | `/jobs?view=summary`, `/jobs/{id}/status`, `/jobs/{id}/result`, `/api/job-cancel`, `/api/job-recreate` | code/test/doc | covered | Repowire retry is represented as retry-by-recreate in Weave; terminal jobs can recreate new queued work. |
| `DashboardDialogs.tsx::SpawnDialog` | Spawn from browser using configured backend/path/profile/circle | Danger zone `POST /api/spawn-peer`, `weave_spawn_peer`, config allowlist/trusted argv | code/test/doc | covered | Weave intentionally makes this a Danger zone, not a modal, and denies by default unless `--write` + allowlist/trusted program. |
| `DashboardDialogs.tsx::OrphanPanesList` | List orphan panes and show CLI adoption command | no dashboard route | code | superseded | Weave uses sessions/hooks and spawn birth certificates; orphan-pane browser adoption is repowire-daemon/tmux-specific. Not a dashboard required area. |
| `DashboardDialogs.tsx::SettingsDialog`, `/health` | Health/settings posture, service peers | `/settings`, `/api/settings`, `/health`, settings panel | code/test/runtime/doc | covered | Weave intentionally reports only token-free booleans/counts, never secrets. |
| `page.tsx` `/circles/{circle}/orchestrator` | Orchestrator presence by circle | peers/roles plus mesh memory/orchestrator surfaces | code/doc | superseded | Weave does not run the repowire daemon/circle orchestrator model; equivalent coordination is through sessions/messages/jobs/memory. |
| `daemon/routes/messages.py` `/notify`, `/broadcast`, `/query`, `/response` | Basic mesh messaging | `weave_notify`, `weave_send`, `weave_reply`, broadcast notify/ask, CLI equivalents | code/test/doc | superset | Weave adds idempotency, signed push/pull, TTL/priority/supersede support, and dual backends. |
| `daemon/routes/asks.py` `/ask`, `/answer`, `/ack`, `/ask-many`, blocking questions | Ask lifecycle and structured questions | `weave_ask`, `weave_answer`, `weave_ack`, `weave_ask_many`, PreToolUse approval | code/test/doc | superset | Weave includes structured ask kinds and host PreToolUse approval gating. |
| `daemon/routes/work.py` `/jobs` | Tracked work list/status/result/cancel/retry/run | `weave_job_create/delegate/claim/update/result/show/list/cancel/status`, CLI `job dispatch` | code/test/doc | superset | Durable native jobs with runner dispatch and progress/result timeline. |
| `daemon/routes/schedules.py` | One-shot/recurring schedules | `weave_schedule`, `weave_schedules`, `weave_cancel_schedule`, `weave_tick` | code/test/doc | covered | Native schedules implemented without Python daemon. |
| `daemon/routes/spawn.py`, `spawn.py` | Spawn/kill/restart/switch backend | `weave_spawn_peer`, `weave_kill_peer`, `weave spawn`, `weave kill`, provider-switch CLI | code/test/doc | covered | Restart/switch-backend are composed from kill/spawn/provider switching; destructive operations remain explicit. |
| `daemon/routes/peers.py` registry/touch/offline/doctor | Peer registry and liveness | `sessions`, hooks, peer list in dashboard, wake/stop hook handling | code/test/doc | covered | Weave's peer concept is session/mailbox centric rather than daemon registry centric. |
| `daemon/routes/peers.py` timeline/transcript | Session transcript history | `weave session export/import`, store history/thread/search, `/peers/{name}/transcript` | code/test/doc | covered | Weave transcript is bounded through messages/history rather than repowire runtime transcript DB. |
| `daemon/routes/attachments.py` | Binary attachment upload/download | none | code | gap | True feature gap, but outside the required dashboard parity list. Do not import until product asks for attachment semantics. |
| `daemon/routes/traces.py` | Delivery trace lookup | `weave_delivery`; `Store::list_delivery`; delivery trace rows | code/test/doc | covered | Weave has metadata-only delivery traces keyed by message id. It is MCP/CLI parity rather than a browser `/traces/{id}` clone, and intentionally excludes message bodies/secrets. |
| `daemon/routes/reviews.py` | Review queue CRUD | `weave_review_queue`, `weave_review_add`, `weave_review_mark`, `weave_review_remove`; `Store::{add_review_item,review_queue,mark_reviewed,remove_review_item}` | code/test/doc | covered | Review queue parity exists in CLI/MCP/store, not in the browser dashboard. This row was corrected during the post-completion /review pass. |
| `daemon/routes/shares.py`, `relay/server.py` shares | Hosted share tokens/viewer | `weave push`, dashboard/server bearer auth, signed intent federation | code/doc | superseded | Weave intentionally avoids hosted relay runtime; ADR-0005-style signed push and owner-only writes replace relay isolation. |
| `relay/server.py` `/events/stream` hosted bridge | Hosted dashboard/relay SSE | local `/events/stream` and cross-machine push/pull | code/runtime/doc | superseded | No hosted relay process is adopted. |
| `hooks/pretooluse_handler.py` | Dangerous tool approval through ask primitive | `weave hook pretooluse`, `pretooluse_is_dangerous`, ask permission flow | code/test/doc | covered | Weave gates native host mutators and weave dangerous tools. |
| `hooks/*` session/prompt/stop/chat_delta/tmux | Provider/runtime hook ingestion | `weave setup`, `weave hook session|prompt|stop|pretooluse`, injection/session modules | code/test/doc | covered | Chat-delta exact UI is not cloned; core hook lifecycle exists. |
| `installers/*` provider installers | Provider setup for multiple hosts | `weave setup --provider claude|codex|gemini|aider`, provider-switch | code/test/doc | covered | Different supported provider set; no Python installer dependency. |
| `memory.py`, `orchestrator/template/SOUL.md` | Memory/persona/SOUL | `weave_memory_*`, `weave-core/src/memory.rs`, session export/import | code/test/doc | covered | SOUL template scaffolding is not copied, but memory primitive exists natively. |
| `mcp/server.py` | Repowire MCP tools/HTTP MCP | `weave-mcp/src/mcp.rs`, `weave server`, `weave dashboard --write` JSON-RPC | code/test/doc | covered | Weave keeps a smaller standing MCP surface and consolidated dispatcher. |
| `channel/server.ts` | TypeScript channel MCP helper | no TS runtime | code/doc | superseded | Explicitly non-goal under Rust-native invariant. |
| `telegram/bot.py`, `slack/bot.py` | Bot bridges | `weave telegram`, `weave slack` | code/test/doc | covered | Poll bridges route through shared dispatcher. |
| `config/*`, `daemon/state/*` | YAML config and state DB/json stores | `weave-core/src/config.rs`, sqlite/libsql store, backup/restore | code/test/doc | superset | Weave has dual backend, backup/restore, and stronger no-shell/bearer gates. |
| `web/app/page.tsx`, marketing components | Public marketing site | docs/README, no runtime web marketing clone | doc | superseded | Not part of Weave runtime/product parity. |

## Dashboard required-area conclusion

| Required area | Result | Evidence |
|---|---|---|
| peer roster | covered | `render_dashboard` section, `/peers` route. |
| selected peer view | covered | `render_selected_peer_detail`, `/peers/{name}/transcript`. |
| transcript/thread search/reply/pagination | covered | transcript endpoint with `q`/`before`, `/api/reply`; integration coverage referenced in WL-083 plan. |
| mesh feed/event stream/reconnect/gap recovery | covered | `/api/events`, `/events/stream`, `/events?since=...`, `DASHBOARD_SCRIPT`. |
| pending questions: choice/tool/free-text | covered | `AskKind` rendering and `/api/answer`. |
| jobs: list/detail/result/cancel/retry/recreate | covered | `/jobs?view=summary`, `/jobs/{id}/status`, `/jobs/{id}/result`, cancel/recreate forms. Retry semantics are retry-by-recreate. |
| settings/config surface | covered | Settings panel plus `/settings` and `/api/settings`. |
| spawn/kill/dialog controls and safety posture | covered | Danger zone spawn/kill forms with explicit `--write`, bearer/cookie, allowlist, trusted-program posture. |
| auth/token/cookie/write gating | covered | `serve_dashboard` and `handle_dashboard_connection` enforce route auth and refuse POST unless `--write`; routable bind requires token. |
| JSON/SSE endpoint compatibility | covered | Repowire-style endpoint names present for the dashboard-critical read APIs. Exact unsupported upstream daemon endpoints are classified above. |

No high-priority dashboard parity gap was found in the required areas. True gaps discovered by the local zip audit are non-required/non-dashboard or intentionally superseded by Weave's Rust-native/no-daemon design: binary attachments, orphan-pane browser adoption, and peer-scoped MCP CRUD. A post-completion review corrected the trace and review-queue rows to covered because Weave already has `weave_delivery` plus `weave_review_*`/store support.

## Current verification commands

Executed on 2026-06-26 after creating this artifact:

```bash
cargo fmt --all --check
cargo clippy -p weave --features surfaces --all-targets -- -D warnings
cargo test -p weave-mcp --features surfaces
cargo test -p weave --features surfaces --test integration surfaces_dashboard -- --nocapture
```

Result: all four commands passed. `weave-mcp` reported 33 unit tests passed; `weave` integration filter `surfaces_dashboard` reported 15 tests passed, 230 filtered out.

## Decision

The local source audit supports the current claim that Weave's **Rust-native dashboard parity** covers repowire's real dashboard operator workflow without adopting the Next.js/Python daemon runtime. The remaining gaps are either explicit non-goals/superseded surfaces or lower-priority adjacent features, not blockers for the WL-083 dashboard parity claim. Post-completion /review found and fixed two audit-classification errors: delivery traces and review queue are covered by existing Weave MCP/store surfaces, even though they are not browser dashboard clones.
