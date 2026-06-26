# WL-082 plan — real terminal dashboard TUI + command-intelligence coverage

Status: planned after `.handoff` resync/migration on 2026-06-26.

## Evidence gathered first

- `git fetch --all --prune` left `develop` equal to `origin/develop` at `88a3917`.
- `hf doctor` was initially DEGRADED because `weave/.handoff/ledger.db` was a legacy C-SQLite ledger while current `hf` expects redb.
- Ran the documented one-time migration build from `meta/handoff`:
  `cargo run -p hf --bin hf --features legacy-sqlite -- migrate /home/drdave/Desktop/meta/weave/.handoff/ledger.db`.
- Migration result: redb ledger installed, legacy SQLite backed up to
  `/home/drdave/.local/share/handoff-ledger-backups/home_drdave_Desktop_meta_weave_.handoff_ledger.db.sqlite.bak`.
- Post-migration `hf doctor`: HEALTH OK, replay OK, witness chain OK.
- `hf sync --dry-run`: Weave would roll up 0 events; other repos still have legacy ledgers and are external to this Weave slice.
- `weave --help` currently advertises 64 top-level commands.
- `weave graph --help` confirms the graph-intelligence surface: connected components, centrality, density over message communication.
- Existing surfaces are not enough for the user request:
  - `weave sessions --watch` is a plain re-rendering text dashboard, read-only and presence-focused.
  - `weave dashboard` is HTTP/SSE behind the `surfaces` feature.
  - The requested target is a real terminal dashboard TUI like `icm dashboard`.

## Problem

Weave has many powerful commands but no first-class terminal operator cockpit. Operators must stitch together `sessions`, `peers`, `scan`, `doctor`, `asks`, `job`, `delivery`, `graph`, and `watch`. That makes stale-peer cleanup, worker orchestration, graph-intelligence review, and command discovery harder than it should be.

## Proposed command

Add a new `weave tui` command, or promote `weave dashboard --terminal` if the CLI design prefers one dashboard namespace. Recommended: `weave tui` because `weave dashboard` already means HTTP/SSE in the `surfaces` feature.

Required modes:

1. `weave tui` — interactive terminal dashboard.
2. `weave tui --once` — deterministic single-frame render for tests and headless logs.
3. `weave tui --json` — machine-readable snapshot for agents.
4. `weave tui --pane sessions|messages|asks|jobs|graph|doctor|leases|commands` — direct pane selection.
5. `weave tui --filter <text>` — narrow peers/messages/jobs/commands.
6. `weave tui --no-color` — stable output and accessibility.

## TUI panes

- Overview: doctor summary, backend, db path, routing anomalies, online/stale counts.
- Sessions: same dimensional truth as `peers`/`scan` (process/pane/reachable/responsive/stale reason), not a folded status only.
- Messages: inbox/recent activity with unread counts and thread hints.
- Asks: open/answered/acked asks plus delivery/response status.
- Jobs: queued/claimed/running/completed/cancelled board with assignee/attempt.
- Graph intelligence: `weave graph` output summarized as nodes/edges/components/density/top central peers, with a route to JSON detail.
- Leases: active reservations and owner/ttl.
- Commands: searchable command catalog grouped by domain, showing whether each command has help smoke, behavior tests, MCP parity, dashboard/TUI exposure, and dangerous/write classification.

## Dependency posture

Start dependency-light and Rust-native:

- Preferred first slice: std-only alternate-screen-free renderer reusing existing pure render seams, with `--once` and polling loop. This can ship with no new dependency.
- If a richer TUI library is justified, gate it behind a default-off feature such as `tui` and document why `ratatui`/`crossterm` is worth the dependency cost. Do not pull it into the default shippable binary without explicit ADR/update.

## Test plan

Immediate coverage added in this slice:

- `every_top_level_command_has_documented_help` in `weave/tests/integration.rs` enumerates all 64 top-level commands from `weave --help` and exercises each command's help path. This is the baseline "test for each command" smoke contract.

Next implementation coverage:

- Unit-test pure TUI snapshot projection from store views.
- Unit-test pane rendering for empty, populated, stale/misregistered, and graph-heavy states.
- Integration-test `weave tui --once --no-color` against an isolated `WEAVE_DB` seeded with peers/messages/asks/jobs/leases.
- Integration-test `weave tui --pane graph --once --json` matches `weave graph --json` counts for the same store.
- Add command behavior tests by domain, not only help smoke:
  - messaging: send/notify/reply/thread/receipts/delivery/search/inbox/outbox/pull;
  - presence: register/attach/peers/sessions/scan/connect/doctor/status/describe/peer-policy;
  - orchestration: ask/answer/ack/ask-many/job/orchestrator/responder/permission/lease;
  - scheduled/daemon/hook/review/memory/session/backup/restore/export/graph/config/provider-switch;
  - dangerous/external: inject/spawn/kill/setup/uninstall/serve/mcp/completions/man/harness with safe hermetic fakes or help-only where execution is intentionally unsafe.

## Upgrade suggestions

1. Add a generated command catalog in code so CLI, MCP, docs, and TUI use one command taxonomy.
2. Add a command-coverage gate that fails when a top-level command lacks help smoke, behavior coverage label, and docs/TUI classification.
3. Feed `graph` intelligence into operator decisions: central peer, isolated components, stale hubs, and high-unread routes should be visible in the TUI overview.
4. Add a stale-session cleanup workflow that previews exact zellij/tmux targets and refuses coarse zellij kills unless the target session is not current and has no live child processes.
5. Keep `sessions --watch` as the low-dependency text view, but make `weave tui` the real cockpit.

## Non-goals for first TUI slice

- Do not replace the HTTP/SSE dashboard.
- Do not make the default build heavier without a feature-gated ADR.
- Do not add write actions before read-only panes and deterministic tests are green.

## Dashboard icon trace (2026-06-26)

The user pointed at `/home/drdave/Desktop/weave-x86_64` as the broken dashboard icon/artifact. Evidence:

- `/home/drdave/Desktop/weave-x86_64 --version` reports `weave 0.1.0` / sqlite.
- `/home/drdave/Desktop/weave-x86_64 --help` exposes only the early seed command set (`mcp`, `setup`, `send`, `inbox`, `peers`, `sessions`, etc.).
- `/home/drdave/Desktop/weave-x86_64 dashboard --help` fails with `unrecognized subcommand 'dashboard'`.
- Current default `~/.cargo/bin/weave` reports `weave 0.2.0`, but a default build still does not expose the HTTP `dashboard` subcommand because that surface is behind `--features surfaces`.

Conclusion: a desktop icon/launcher that expects `weave dashboard` is stale in two ways: the Desktop binary is prehistoric, and the HTTP dashboard is not a default-build command. The durable fix is not to revive the old Desktop ELF; it is to add a default-build terminal operator cockpit (`weave tui`) and keep the HTTP dashboard feature-gated.

Implemented first slice:

- `weave tui` in the default binary.
- `--once` for icon/headless/test launchers.
- `--json` machine-readable snapshot.
- `--pane overview|sessions|messages|asks|jobs|graph|leases|commands`.
- `--filter <text>` and `--no-color`.
- Graph pane reuses the same graph-intelligence summary as `weave graph`.
- The overview explicitly states: `HTTP dashboard is feature-gated; this TUI is default-build.`
