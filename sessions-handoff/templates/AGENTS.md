# AGENTS.md

## First command

Run:

```bash
hf resume
```

## Mission

Maintain this repository through the Ark Handoff Ledger protocol. The repo is the source of truth. Chat history is not authoritative.

## Hard rules

- Do not edit files without a task claim.
- Do not write outside claimed path scope.
- Do not run a parallel write session against overlapping paths.
- Do not mark a task complete without tests or an explicit waiver.
- Do not stop without `hf checkpoint` and `hf handoff`.
- Do not make architecture changes without an ADR.
- Do not treat `.handoff/packets/latest.md` as more authoritative than Git, the ledger, or task cards.

## Required before stopping

```bash
hf checkpoint
hf drift
hf handoff
```

## Navigation order

1. `.handoff/active.md`
2. `.handoff/context/capsule.json`
3. `docs/AGENT_NAVIGATION.md`
4. `.handoff/packets/latest.md`
5. `.handoff/tasks/active/`
