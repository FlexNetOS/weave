# HANDOFF — weave (multi-surface write/commands session)

closed_utc: 2026-06-13T23:50Z
branch: develop @ 0137af7 (master syncing via sync-master)
worktree: main checkout + 2 session worktrees still present (see cleanup below)
last_item: WL-052a (dashboard write) + WL-052b (bot commands) — in PR #88 (armed, not yet merged)
next_item: shepherd #88 to merge, then WL-052b Slack wiring / WL-052a HTML form
gate_status: PASS locally — 610 surfaces tests; clippy clean default/libsql/surfaces/libsql+surfaces

## Landed / in-flight this session ("knock out the next three")
- #87 ci: concurrency group + push scoped to master/develop — **MERGED** (kills the
  duplicate-run flake that forced reruns all last session).
- #88 feat(surfaces): **WL-052a dashboard write** + **WL-052b bot commands** — **ARMED auto-merge,
  updated onto develop, NOT yet merged.** Both obey the one-handler-many-surfaces law.

## What #88 contains
- **WL-052a:** `weave dashboard --write` → bearer-gated `POST /api` JSON-RPC route dispatched
  through the SAME `dispatch_request → call_tool` as MCP/CLI (caps, params! SQL, destructive
  gating, nudge-inject inherited). Read-only default (POST → 403). +2 integration tests.
  Files: `weave-mcp/src/http.rs` (serve_dashboard + handler), `weave-mcp/src/mcp.rs`
  (`PullConsent::empty()`), `weave/src/main.rs` (Cmd::Dashboard `--write`).
- **WL-052b:** Telegram `/inbox` `/peers` `/sessions` `/help` via the same handler; ordinary
  text still relays. Read-only v1 (mutating cmds hit `dangerous=false`). Pure parser/mapper/
  formatter + 3 unit tests. File: `weave/src/telegram.rs`.

## RESUME — do this first
1. `cd` main checkout, `git fetch`. If #88 still open + BEHIND: `cd ../weave-wl052a-dash`,
   `git merge origin/develop && git push` to re-arm (merge-train — each PR goes BEHIND as
   siblings merge; the CI concurrency fix is now in, so flakes should drop).
2. After #88 merges: **clean up worktrees** — `git worktree remove ../weave-ci-concurrency
   ../weave-wl052a-dash ../weave-hf2 --force; git worktree prune`.
3. verify-on-resume: `cargo test --all-targets` (expect ~581) + `--no-default-features
   --features libsql` (~541) + `--features surfaces` (~610).

## Open backlog (owner's pick)
- WL-052b Slack wiring (reuse the telegram grammar — `parse_bot_command`/`bot_command_rpc`/
  `format_bot_reply` are surface-agnostic) + `/ask`/`/send` write commands.
- WL-052a HTML send form + per-message/job read views (the API is the substrate; form is a leaf).
- WL-052 design law (durable, ICM): a human surface routes to the SAME handler as CLI/MCP, never
  a parallel impl. Also: deny `git push`/`gh pr` to weave-* subagents; WL-043 single-crate (deferred).

## Cross-repo inbox (NOT weave tasks — parked on owner decisions)
- #96 lane: multi-hop vs pivot to Phase A1 obscura. #97 harness_hub: use-the-harness (envctl-kasetto
  verify, archon-port). #98 handoff: HFTASK-0033 (fleet-ledger-427 + hf-sync-confirm).

## verify_on_resume
- `git fetch origin && git status --porcelain` (main checkout clean)
- `cargo test --all-targets` && `cargo test --no-default-features --features libsql` && `cargo test --features surfaces`

resume_command: /session-relay resume
