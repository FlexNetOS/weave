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

1. Run `hf resume --json`.
2. Read `.handoff/active.md`.
3. Read `.handoff/context/capsule.json`.
4. Read active task card.
5. Check latest drift report.
6. Print exact next command.

## Hard rule

Do not edit files during this skill.
