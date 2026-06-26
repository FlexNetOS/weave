# WL-083 plan — real repowire dashboard/UI parity for Weave

Status: active after the user challenged the minimal Weave web UI on 2026-06-26.

## Evidence from prior handoff

- `.handoff/decisions/ADR-0004-rust-native-human-surfaces.md` says the target was repowire's human surfaces: Next.js dashboard plus Telegram/Slack, but locked Weave to Rust-native server-rendered HTML + SSE, no Next.js/Node runtime.
- `.handoff/loop/WL-052_changes.md` says the read-only dashboard and bots were only a v1 baseline; dashboard write forms and bot command grammar were intentionally decomposed into follow-up work.
- `.handoff/loop/_done/_workspace_prev/references/features/prassanna-ravishankar--repowire.md` identified the Browser dashboard as a repowire human surface but originally ranked it low-impact and said `sessions --watch` covered most of it. That is now too weak for the owner's expectation.
- `docs/REPOWIRE-PARITY.md` previously overstated Browser dashboard as HAVE; this slice changes it to PARTIAL.

## Evidence from upstream repowire

Fresh clone of `github.com/prassanna-ravishankar/repowire` on 2026-06-26 shows the actual web UI is under `web/app/dashboard/**`, about 7.9k lines of dashboard code/tests:

- `page.tsx`: app shell with peer roster, mesh feed, selected peer view, pending questions, jobs view, mobile tabs, settings dialog, spawn dialog, and event stream wiring.
- `components/PeerView.tsx`: selected-peer conversation/timeline/transcript/search/MCP/session-control/compose surface; posts `/notify`, `/ask`, `/answer`, `/reply`, session control routes, attachments, and MCP routes.
- `components/JobsView.tsx`: job summary, selected job detail, retry/cancel actions.
- `components/DashboardDialogs.tsx`: spawn/settings/orphan-pane control surfaces.
- `lib/useEventStream.ts`: `/events/stream` plus reconnect gap recovery via `/events?since=...`.
- Expected read endpoints include `/peers`, `/events`, `/events/stream`, `/jobs?view=summary`, `/jobs/{id}/status`, `/circles/{circle}/orchestrator`, `/health`, plus write/control endpoints.

## Current Weave state before WL-083

- `weave dashboard` was a read-only server-rendered table page: sessions, recent messages, jobs, leases, schedules.
- It had `GET /` and `GET /events` (HTML/SSE) only, plus optional `POST /api` when launched with `--write`.
- It did not expose repowire-style JSON read endpoints or the multi-pane app shell.

## This slice

Implemented the first aligned Rust-native slice toward the real repowire UI while preserving ADR-0004's no-Next.js/no-Node constraint:

- Upgraded `render_dashboard` into a repowire-inspired three-column operator shell:
  - Peer roster with counts and cards.
  - Mesh feed event cards.
  - Control plane with the legacy tables retained for compatibility.
- Added browser/read API endpoints:
  - `GET /api/snapshot` with `repowire_compat: true`.
  - `GET /peers` repowire-style peer roster envelope.
  - `GET /api/events` mesh feed event JSON.
  - `GET /jobs?view=summary` job summary envelope `{work, recurring}`.
  - `GET /health` dashboard health.
- Kept existing `GET /events` SSE behavior for backwards compatibility and added `/events/stream` as the repowire-style SSE path.

## Remaining work to call the dashboard parity complete

## 2026-06-26 follow-up slice

Added the next Rust-native parity slice after the initial shell:

- Selected peer panel now renders session id, role/turn state, cwd/repo/branch,
  description, and a bounded transcript preview for the selected peer.
- Pending questions panel now renders open tracked asks.
- Snapshot/read API now includes `asks` and `pending_questions`.
- Added repowire-style read endpoints:
  - `GET /asks/pending`
  - `GET /peers/{name}/transcript`
  - `GET /jobs/{id}/status`
- Verification added to the surfaces dashboard integration test for asks and
  transcript endpoints, plus browser/curl smoke evidence under `.handoff/run/`.

## 2026-06-26 action-form slice

Added the first dashboard write/control slice without introducing any dashboard-local
mutation logic:

- Dashboard now renders Notify, Ask, and Answer browser forms in an Actions panel.
- Browser query-token loads set the existing `weave_dashboard_token` cookie so
  forms can authenticate without an `Authorization` header.
- `POST /api/notify`, `POST /api/ask`, and `POST /api/answer` parse standard
  form-url-encoded bodies and adapt them into the existing JSON-RPC
  `tools/call` envelope.
- The actual writes still route through `dispatch_request` and the canonical
  `weave_notify`, `weave_ask`, and `weave_answer` tool implementations; read-only
  dashboards still reject POST unless launched with `--write`.
- Integration coverage proves notify delivery, ask creation, and answer closure
  through the form endpoints; runtime curl and Playwright evidence are under
  `.handoff/run/repowire-action-forms/`.

## 2026-06-26 job-control slice

Extended the action-form work into the jobs panel:

- Job cards now render a cooperative Cancel form for non-terminal jobs.
- `POST /api/job-cancel` adapts form-url-encoded input into the existing
  `weave_job_cancel` JSON-RPC tool call.
- Integration coverage proves a dashboard job-cancel form updates canonical job
  state and that `GET /jobs/{id}/status` reports the cancelled job.

Do not mark repowire dashboard parity complete until these are true and verified:

1. Selected peer detail view: full transcript/history pagination, thread/timeline search, rich selected-peer message rendering, and current active session controls.
2. Pending questions panel: structured choice/tool-permission rendering and answer UX beyond the basic answer form.
3. Write forms: reply forms and richer notify/ask/answer affordances; current notify/ask/answer forms route through the single `dispatch_request` path with no dashboard-local mutation logic.
4. Jobs panel parity: selected job detail plus retry/recreate; cooperative cancel is now routed through the shared handler.
5. Settings/spawn controls: if adopted, spawn stays allowlist/birth-cert/argv-only and surfaces clear dry-run/preview where possible.
6. Event stream gap recovery: `/events/stream` plus `/events?since=...` or an equivalent documented route.
7. Playwright/browser smoke: opened page visibly contains peer roster, mesh feed, selected-peer or placeholder, jobs/control plane; JSON endpoints return expected shapes.
8. Docs parity matrix stays honest: Browser dashboard remains PARTIAL until the above surfaces exist.
