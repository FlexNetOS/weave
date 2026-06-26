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

## 2026-06-26 event-gap recovery slice

Closed the first event-stream recovery gap from upstream repowire:

- `GET /events` remains the long-lived SSE route for live browser updates.
- `GET /events?since=<id>` now returns JSON events for reconnect/gap recovery,
  matching the upstream dashboard client's `/events?since=...` pattern.
- `GET /api/events?since=<id>` uses the same filter.
- Event ids accept both numeric ids and the existing `msg_<id>` JSON id shape.
- Integration coverage proves `/events?since=0` returns JSON with missed events
  and a high-water `since` filters already-seen events.

## 2026-06-26 selected-peer transcript slice

Expanded the selected-peer surface toward upstream `PeerView.tsx`:

- Selected peer transcript preview now renders a Reply form for the latest visible
  message.
- `POST /api/reply` adapts form-url-encoded input into canonical `weave_reply`
  via the existing `dispatch_request` JSON-RPC path.
- `GET /peers/{name}/transcript` now supports `q=<query>` search and
  `before=<id>` pagination filters, while keeping the existing transcript JSON
  shape (`turns`, `next_before`).
- Integration coverage proves form reply delivery and transcript search/no-match
  behavior.

## 2026-06-26 structured pending-question slice

Closed the basic structured-ask UX gap in the pending questions panel:

- Choice asks now render one answer form/button per option.
- Tool-permission asks now render approve/deny buttons.
- Free-text asks now render an inline answer form.
- All controls post to the existing `/api/answer` adapter and route through
  canonical `weave_answer`; no dashboard-local ask mutation logic was added.
- Integration coverage proves choice and tool-permission answers go through the
  dashboard form surface and leave the pending list.

## 2026-06-26 selected-peer session-control slice

Added the first non-destructive selected-peer session controls:

- Selected peer details now render Session controls for turn state and description.
- `POST /api/turn-state` adapts form-url-encoded input into canonical
  `weave_set_turn_state`.
- `POST /api/description` adapts form-url-encoded input into canonical
  `weave_set_description`.
- Both controls route through the same `dispatch_request` JSON-RPC path used by
  MCP/CLI and remain gated by `weave dashboard --write`.
- Integration coverage proves the forms update canonical peer fields visible from
  `GET /peers`.

## 2026-06-26 job recreate slice

Extended jobs-panel controls beyond cancellation:

- Terminal job cards now render a Recreate form.
- `POST /api/job-create` adapts form-url-encoded input into canonical
  `weave_job_create`.
- Recreate remains a new queued job (retry-by-recreate) rather than mutating a
  terminal job, preserving the board lifecycle invariant.
- Integration coverage proves a cancelled job renders the recreate control and
  the jobs summary includes the new retry job after submit.

## 2026-06-26 browser reconnect slice

Closed the browser-side half of event recovery:

- The rendered dashboard now opens `EventSource('/events/stream')`.
- On browser load and EventSource errors, it fetches `/events?since=<lastSeen>`
  with same-origin credentials for gap recovery.
- Query-token page loads still set the existing dashboard cookie, so the browser
  reconnect/recovery calls reuse the same auth path.
- Integration coverage asserts the browser page contains the reconnect wiring;
  runtime smoke verifies `/events?since=0` returns the seeded event.

Do not mark repowire dashboard parity complete until these are true and verified:

1. Selected peer detail view: non-destructive turn-state/description controls are now present; remaining work is richer timeline/thread rendering and dangerous session controls (kill/spawn) with clear preview/allowlist posture.
2. Pending questions panel: basic choice/tool-permission/free-text answer controls are now present; remaining work is richer validation/status and tool-args display.
3. Write forms: richer notify/ask/answer/reply affordances; current notify/ask/answer/reply forms route through the single `dispatch_request` path with no dashboard-local mutation logic.
4. Jobs panel parity: selected job detail remains; cooperative cancel and retry-by-recreate are now routed through the shared handler.
5. Settings/spawn controls: if adopted, spawn stays allowlist/birth-cert/argv-only and surfaces clear dry-run/preview where possible.
6. Event stream gap recovery: `/events/stream` SSE plus browser-side `/events?since=...` recovery is now present; remaining work is richer event typing.
7. Playwright/browser smoke: opened page visibly contains peer roster, mesh feed, selected-peer or placeholder, jobs/control plane; JSON endpoints return expected shapes.
8. Docs parity matrix stays honest: Browser dashboard remains PARTIAL until the above surfaces exist.

## 2026-06-26 selected-job detail slice

Expanded the jobs panel toward upstream selected-work detail:

- The main dashboard now renders a Selected job panel for the newest job, with
  lifecycle fields (state/kind/owner/assignee/phase), timing, cancellation,
  description/prompt, progress note, result summary, and progress timeline.
- The selected-job panel links the read APIs for `/jobs/{id}/status` and the new
  `/jobs/{id}/result` endpoint.
- `GET /jobs/{id}/result` exposes the canonical store `job_result` view as JSON,
  including `ready`, terminal state, result summary, and terminal payload fields.
- Integration coverage proves the selected-job HTML renders progress/result
  details and that both status/result endpoints expose the richer job data.
