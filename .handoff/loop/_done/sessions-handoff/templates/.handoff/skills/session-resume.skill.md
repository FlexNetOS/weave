# Skill: session-resume

## Purpose

Rehydrate current project state for a new agent session with minimal context load.

## Trigger phrases

- resume
- continue
- pick up
- what next
- recover session

## Steps

1. `cd` to the loop worktree (`/home/drdave/Desktop/meta/weave-mcp-daemon-tools`).
2. Run `bash _workspace/verify-on-resume.sh`.
3. Read `_workspace/HANDOFF.md`.
4. Read `_workspace/backlog.md`.
5. Read `_workspace/loop_state.md`.
6. Check for `_workspace/STOP` — if exists, halt.
7. Check for `_workspace/DONE` — if exists and no new gaps, report completion.
8. Identify first `- [ ]` item in backlog.
9. Read relevant `docs/ROADMAP-v0.X.md` section.
10. Print exact next command.

## Hard rule

Do not edit files during this skill. This is read-only state reconstruction.
