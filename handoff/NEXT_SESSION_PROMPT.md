# Next-session pickup prompt — weave-loop

> Paste the block below into a fresh Claude Code session at the loop worktree.
> Everything above the line is orientation for you (the human); the fenced block is the prompt.

---

```
You are resuming weave development. weave is a single static Rust binary that lets
coding-agent sessions message each other over a shared SQLite/libSQL mailbox and
push messages into a recipient's terminal pane via a native multi-mux injector
(tmux, zellij, kitty, wezterm, screen). No Python, no daemon required.

Operate via the `weave-orchestrator` skill for any code change (it runs the
mandatory fresh-worktree + Rust-native drift scan first, then the planner →
implementer → verifier → guardian team, with the dual-backend gate:
fmt + clippy -D warnings + test on BOTH sqlite and libsql).

## Where things stand

**All M1/M3 backlog items are complete (WL-001..WL-013).**

Recently delivered:
- PR #30 (82ea6dd) — Workspace split: weave-core, weave-inject, weave-mcp, weave bin
- PR #32 — WL-002 Phase A: presence daemon store + CLI
- PR #33 — WL-002 Phase B: MCP daemon tools
- PR #50 — WL-003 zellij pane targeting, WL-004 daemon integration tests, WL-005 ralph-weave.sh hardened
- PR #50 also contained WL-006 setup, WL-007 bracketed-paste hardening
- PR #51 — WL-010: retirement of mcp-broker / repowire
- PR #52 — WL-011: presence daemon marked duplicate of WL-002
- PR #53 — WL-012: mux adapters marked duplicate of inject.rs
- WL-013 — Config file with mux_preference

**Validated on live target boxes:**
- WL-008 — Live zellij injection validated; bug fixed (zellij liveness probe ANSI codes)
- WL-009 — weave 0.2.0 built and setup on RTX-5090 box (2× RTX 5090)

**Reference repo tracking system deployed** at `_workspace/references/`.
6 repos cross-referenced (repowire, mcp_agent_mail, atm-core, cross_agent_session_resumer,
claude-code-router, cc-mirror). 29 new gaps identified and added to backlog (WL-014..WL-042).

## Current worktree
- **Path:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Branch:** `feat/zellij-pane-targeting`
- **Next item:** WL-014 — Reminder injection for open asks
- **Alternative next items:** WL-028 (FTS5 search), WL-015 (structured questions), WL-029 (file leases)

## Top new gaps (ranked by impact)
1. **WL-014** — Reminder injection for open asks (HIGH)
2. **WL-015** — Structured question types: choice, free-text, tool-permission (HIGH)
3. **WL-028** — FTS5 full-text search on messages (HIGH)
4. **WL-029** — Advisory file leases with TTL expiry (HIGH)
5. **WL-016** — Scheduler / cron for messages (MEDIUM)
6. **WL-017** — Mesh memory system (MEDIUM)
7. **WL-018** — Birth certificates / runtime identity envelopes (MEDIUM)
8. **WL-019** — Co-orchestrator support (MEDIUM)
9. **WL-035** — Mailbox backup / restore (MEDIUM)
10. **WL-036** — Post-send hooks (MEDIUM)

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
nothing specific, propose WL-014 (reminder injection) as the highest-impact next feature.
```

---

## Quick-reference facts (for the human)
- **Repo:** `/home/drdave/Desktop/meta/weave` (original) · **Worktree:** `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
- **Branch:** `feat/zellij-pane-targeting` (loop worktree)
- **Remote:** `https://github.com/FlexNetOS/weave.git`
- **All M1/M3 tasks:** DONE ✅ (WL-001..WL-013)
- **New gaps:** WL-014..WL-042 (29 items from cross-reference scan)
- **Reference tracking:** `_workspace/references/MANIFEST.md`
- **Verify on resume:** `bash _workspace/verify-on-resume.sh`
