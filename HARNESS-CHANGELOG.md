# weave — Harness change history

Tailoring the envctl-pattern autonomous harness to `weave`. Each row is one
git-tracked commit on `harness/weave-loop`. (Per HARNESS-UPGRADE-KIT §9.)

| Date (UTC) | Commit | Target | Reason |
|------------|--------|--------|--------|
| 2026-06-06 | (this) | `_workspace/{backlog.md, loop_state.md, verify-on-resume.sh}` | Seed durable state under `harness/weave-loop` worktree. |
| 2026-06-06 | (this) | `.claude/skills/weave-loop/SKILL.md` | In-session body: DISCOVER / RESUME / CYCLE / DONE / NEEDS-HUMAN. |
| 2026-06-06 | (this) | `.claude/skills/session-relay/SKILL.md` | HAND OFF + RESUME entry points; bootstrap-hazard rule. |
| 2026-06-06 | (this) | `.claude/agents/continuity-steward.md` | Offload cold-start `HANDOFF.md` to a fresh agent. |
| 2026-06-06 | (this) | `.claude/commands/weave-loop.md` | `/weave-loop resume from _workspace/HANDOFF.md` slash command. |
| 2026-06-06 | (this) | `.claude/skills/weave-loop/scripts/ralph-weave.sh` + `README.md` | External `/new` runner; SAFE default, `WEAVE_APPLY=1` opt-in, `STOP`/`MAX_ITERS` backstops. |
| 2026-06-07 | (this) | `.claude/skills/weave-loop/scripts/ralph-weave.sh` + `README.md` | Coordinated logged-in Kimi Code K2.6 session (`kimi-legacy -r 3c6e...`) preflight/review around Ollama Cloud MiniMax implementation passes. |

## Bootstrap hazard (single rule, repeated)

The loop's `session-relay` heartbeat (`relay:handoff` / `relay:resumed`, `to:"all"`)
runs over weave itself. If a cycle changes weave's wire/inbox behavior, **do not
depend on the live `weave` binary for the handoff** that cycle. The committed
`_workspace/HANDOFF.md` is the authoritative resume signal anyway. Pin a known-good
`weave` on `PATH` for the relay, or skip the heartbeat. Re-verify the heartbeat
*after* the build passes, before handing off.
