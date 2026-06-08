# Next-session pickup prompt — weave-loop

> Paste the block below into a fresh Claude Code session at the loop worktree.
> Everything above the line is orientation for you (the human); the fenced block is the prompt.

---

```
You are resuming weave development. weave is a single static Rust binary that lets
coding-agent sessions message each other over a shared SQLite/libSQL mailbox and
push messages into a recipient's terminal pane via a native multi-mux injector
(tmux, zellij, kitty, wezterm, screen, iTerm2). No Python, no daemon required.

Operate via the `weave-orchestrator` skill for any code change (it runs the
mandatory fresh-worktree + Rust-native drift scan first, then the planner →
implementer → verifier → guardian team, with the dual-backend gate:
fmt + clippy -D warnings + test on BOTH sqlite and libsql).

## Where things stand

**M1/M3 backlog complete (WL-001..WL-013).**
**Gaps backlog heavily progressed (WL-014..WL-027 done).**

Recently delivered:
- WL-014..WL-025 — Reminder injection, structured questions, scheduler, mesh memory,
  birth certificates, co-orchestrator, review queue, tool approval, HTTP MCP transport,
  iTerm2 backend, reservation leases, stop-boundary wake.
- WL-026 — Idempotency keys & trace IDs (PR #57, auto-merge).
- WL-027 — Broadcast notify / broadcast ask (`weave_broadcast_notify` + `weave_broadcast_ask`
  MCP tools; `weave broadcast-notify` + `weave broadcast-ask` CLI commands).
- FrankenNetworkX crate extraction — `fnx-classes` + `fnx-algorithms` + `fnx-runtime`
  wired in via Cargo git dependencies. `weave graph` command runs connected_components,
  degree_centrality, and density on the peer/message communication network.

**Current gate:** 510 tests sqlite, 470 tests libsql, fmt + clippy clean.

## Current worktree
- **Path:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Branch:** `feat/zellij-pane-targeting`
- **Next item:** WL-028 — FTS5 full-text search on messages, threads, and subjects
- **Alternative next items:** WL-029 (advisory file leases), WL-030 (pre-commit Git hook),
  WL-031 (message importance/priority levels)

## Top remaining gaps (ranked by impact)
1. **WL-028** — FTS5 full-text search on messages (P0)
2. **WL-029** — Advisory file leases with TTL expiry and conflict detection (P0)
3. **WL-030** — Pre-commit Git hook for file reservation guard (P1)
4. **WL-031** — Message importance / priority levels (P1)
5. **WL-032** — Per-peer contact policies (P1)
6. **WL-033** — Thread summarization via LLM (P1)
7. **WL-035** — Mailbox backup / restore (P1)
8. **WL-036** — Post-send hooks (P1)

See `_workspace/backlog.md` for the complete list (WL-001..WL-042).

## How to work
- ALWAYS start via `weave-orchestrator` for code changes; it enforces the fresh worktree
  (`git worktree add ../weave-<slug> -b <branch>`) and the Rust-native drift scan.
- The dual-backend gate is non-negotiable: sqlite AND `--no-default-features --features libsql`,
  plus `--features sign` when touching crypto.
- Any Store/schema change must be mirrored in BOTH `weave-core/src/store.rs` and
  `weave-core/src/store_libsql.rs` (schema + guarded additive migration + trait methods).
- Commit/PR/merge only when the user asks. Conventional Commits; update CHANGELOG [Unreleased]
  and the relevant docs in the same change.

Start by reading `_workspace/HANDOFF.md` (authoritative resume signal), then
`_workspace/backlog.md` for the full backlog. Run `bash _workspace/verify-on-resume.sh`
to confirm the baseline is green. Ask the user which item to pick — or, if they said
nothing specific, propose WL-028 (FTS5 full-text search) as the highest-impact next feature.
```

---

## Quick-reference facts (for the human)
- **Repo:** `/home/drdave/Desktop/meta/weave` (original) · **Worktree:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Branch:** `feat/zellij-pane-targeting` (loop worktree)
- **Remote:** `https://github.com/FlexNetOS/weave.git`
- **M1/M3 tasks:** DONE ✅ (WL-001..WL-013)
- **Gaps delivered:** WL-014..WL-027 ✅
- **Next:** WL-028 (FTS5 search)
- **Reference tracking:** `_workspace/references/MANIFEST.md`
- **Verify on resume:** `bash _workspace/verify-on-resume.sh`
