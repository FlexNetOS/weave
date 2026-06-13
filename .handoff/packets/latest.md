# HANDOFF — weave (ADR-0003 token-light + multi-surface session, COMPLETE)

closed_utc: 2026-06-13T22:25Z
branch: develop @ b7c13c2 (master syncing via sync-master workflow)
worktree: main checkout /home/drdave/Desktop/meta/weave (all session worktrees removed)
cycle_budget: n/a (interactive, owner asked to "push through the next 4 tasks")
cycles_total: prior 4 cards + 4 runtime fixes, + this session's 4 cards (WL-050/051/052/053)
cycles_this_session: WL-050, WL-051, WL-053, WL-052 — all merged
last_item: WL-052 (multi-surface parity foundation) — merged #85
next_item: WL-052a (dashboard write/read) or WL-052b (bot command grammar) — owner's pick
orchestrator_phase: complete (WL-050 ran plan→implement→verify→guardian APPROVE; others verified inline)
gate_status: PASS — develop==b7c13c2, baseline 581 sqlite / 541 libsql green; all PR checks green
pr_url: (none open) — PRs #82 #83 #84 #85 all merged

## Landed this session (all merged to develop)
- #82 feat(mcp): token-light progressive-disclosure MCP surface (WL-050)
- #83 fix(inject): capture tmux server socket in peer target (WL-053)
- #84 feat(mcp): token-light invariant + standing-token budget gate (WL-051)
- #85 docs(parity): multi-surface parity matrix + WL-052 decomposition (WL-052)

## What each card delivered
- **WL-050 (ADR-0003 keystone).** 73 eager flat `weave_*` tools → ONE standing `weave`
  **meta-tool** (modes `search`/`describe`/`call`/`list`). `tool_catalog()` = canonical 73-op
  registry; `tools()` returns just the meta-tool by default. `call` routes through `call_tool`,
  re-applies the safe-HTTP destructive-op gate to the INNER op, refuses self-recursion.
  Eager-flat fallback: `WEAVE_MCP_EAGER=1`. Guardian APPROVE. +11 tests.
- **WL-051.** `token-light` is now a CLAUDE.md non-negotiable invariant (peer of
  `dependency-light`) + CI budget gate `MAX_STANDING_TOOLS_BYTES=8192`
  (`standing_mcp_surface_is_within_token_budget`). Eager-flat opt-in is exempt. +1 test.
- **WL-053.** Capture `$TMUX` socket at registration (persisted on the **existing**
  `peers.socket` column — **NO schema change**); thread `tmux -S <socket>` through
  inject/spawn/kill/liveness. Socket-less peers keep historical default-server argv. +3 tests.
- **WL-052 (foundation).** `docs/MULTI-SURFACE-PARITY.md` — CLI + MCP at **full parity**;
  dashboard (read-only) + bots (relay) = WL-048 v1. Docs-only; remaining write-parity tracked.

## Decisions (also in ICM: decisions-weave 01KV1HA8…, context-weave 01KV1H9M…, errors-resolved 01KV1HAB…)
- WL-050: chose the **meta-tool** over 14 namespaced dispatchers (hits ≤2k-token target, zero loss, lower risk).
- MCP integration harness (`McpServer::spawn_full`) defaults to `WEAVE_MCP_EAGER=1` so historical
  "advertised in tools/list" assertions verify the compat path; progressive tests pass `WEAVE_MCP_EAGER=0`.
- WL-052 deliberately scoped to a **docs foundation** + WL-052a/b decomposition — human-surface
  WRITE paths are security-sensitive (hand-rolled HTTP POST / chat parser) and not safe to rush.
  Design law for the remainder: a human surface routes to the **same** `tool_*` handler as CLI/MCP.

## Dead-ends / hazards (do not re-trip)
- **Late-test fmt gap:** adding a test AFTER `cargo fmt` leaves it unformatted → CI rustfmt fails
  while tests pass (hit on #82). Re-run `cargo fmt --all` after late test additions. **RTK masks
  the fmt --check exit code** (prints `Diff in …` but exits 0) — grep `Diff in`, don't trust `$?`.
- **CI duplicate-run flakes (STANDING DEBT):** `ci.yml` triggers on push **and** pull_request with
  no `concurrency:` group → two racing runs amplify timing flakes (dashboard readiness, lease sweep,
  federation). Workaround: `gh run rerun <id> --failed`. Recommended fix still open: scope push to
  `[master, develop]` + add `concurrency: {group: ci-${{github.ref}}, cancel-in-progress: true}`.
- **Branch-up-to-date merge-train:** each PR went BEHIND as siblings merged; had to `git merge
  origin/develop` + push to re-arm. Expected with "require branches up to date".
- **`token` substring false-fail:** `peers_json_surfaces_remote_host_peer_alive_remote_additive_keys`
  substring-checks `peers --json` for `"token"` over output incl. the cwd path → false-fails when
  the worktree dir contains "token" (e.g. `weave-wl051-token-budget`). Green on CI. Worth hardening.
- **Agent self-delivery hazard** (unchanged): the LEADER owned all git/PR this session; subagents
  (only the read-only weave-guardian was spawned) did NOT push. Keep it that way.

## icm_stored
- context-weave 01KV1H9MR9…, decisions-weave 01KV1HA8T8…, errors-resolved 01KV1HABHX…

## Open backlog (next session — owner's pick)
- WL-052a: dashboard write/read completeness (bearer-gated POST surface; reuse `tool_*` handlers).
- WL-052b: bot command grammar (`/inbox`/`/ask`/`/peers` → same handlers; secret-free).
- WL-043 single-crate collapse (DEFERRED until meta workspace aligned).
- Standing process fixes still open: (a) deny `git push`/`gh pr` for weave-* subagents; (b) CI concurrency.

## verify_on_resume
- `git fetch origin && [ "$(git rev-parse origin/master)" = "$(git rev-parse origin/develop)" ] && echo converged`  # master catches up to b7c13c2 via sync-master
- `git status --porcelain` empty
- `cargo test --all-targets` (sqlite, expect 581) and `cargo test --no-default-features --features libsql` (expect 541)

resume_command: /session-relay resume
