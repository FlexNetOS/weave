#!/usr/bin/env bash
# ralph-weave.sh — unified weave-loop runner.
# One agent per iteration drives plan→implement→verify.
# MiniMax is the external guardian (review + approve).
# On APPROVE: commit, push, PR create, auto-merge.
# On BLOCK: preserve findings, retry next iteration.
#
# This merges the 3 parts into one closed auto-loop:
#   1) Local agent = planner + implementer + verifier
#   2) MiniMax    = guardian (review + approve)
#   3) Local agent = delivery (commit + PR + auto-merge)
set -euo pipefail

WORKTREE="${WEAVE_WORKTREE:-/home/drdave/Desktop/meta/weave}"
BUDGET="${WEAVE_BUDGET:-3}"
MAX_ITERS="${WEAVE_MAX_ITERS:-50}"
SLEEP_BETWEEN="${WEAVE_SLEEP:-5}"
MODEL="${WEAVE_MODEL:-minimax-m3:cloud}"
GUARDIAN_CMD="${WEAVE_GUARDIAN_CMD:-}"
AGENT_CMD="${WEAVE_AGENT_CMD:-claude}"
AGENT_MODEL_ARGS="${WEAVE_AGENT_MODEL_ARGS:-}"
APPLY="${WEAVE_APPLY:-0}"

WS="$WORKTREE/_workspace"
mkdir -p "$WS"

log(){ printf '[ralph-weave %s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }

read -r -a GUARDIAN_CMD_ARY <<<"$GUARDIAN_CMD"
read -r -a AGENT_CMD_ARY <<<"$AGENT_CMD"
read -r -a AGENT_MODEL_ARGS_ARY <<<"$AGENT_MODEL_ARGS"
command -v "${AGENT_CMD_ARY[0]}" >/dev/null || { log "FATAL: agent '${AGENT_CMD_ARY[0]}' not on PATH"; exit 1; }
[ -d "$WORKTREE" ]   || { log "FATAL: worktree $WORKTREE not found"; exit 1; }
[ -f "$WORKTREE/Cargo.toml" ] || { log "FATAL: $WORKTREE/Cargo.toml missing"; exit 1; }

if [ "${WEAVE_SKIP_GUARDIAN:-0}" != "1" ]; then
  if [ -z "$GUARDIAN_CMD" ]; then
    log "FATAL: WEAVE_GUARDIAN_CMD is not set."
    log "       Example: WEAVE_GUARDIAN_CMD='claude --agent guardian'"
    log "       Or disable the guardian phase by setting WEAVE_SKIP_GUARDIAN=1"
    exit 1
  fi
  command -v "${GUARDIAN_CMD_ARY[0]}" >/dev/null || { log "FATAL: guardian '${GUARDIAN_CMD_ARY[0]}' not on PATH"; exit 1; }
fi

# Phase C needs `gh` for PR creation / auto-merge.
command -v gh >/dev/null || { log "FATAL: gh (GitHub CLI) not on PATH"; exit 1; }

APPLY_ARGS=()
if [ "$APPLY" = "1" ]; then
  APPLY_ARGS=(--dangerously-skip-permissions)
  log "APPLY MODE — will modify the live system unattended."
else
  log "SAFE mode (default): destructive applies refused. Set WEAVE_APPLY=1 to act."
fi

# ---------------------------------------------------------------------------
# Phase A prompt — construction crew: plan → implement → verify
# ---------------------------------------------------------------------------
read -r -d '' PROMPT_PHASE_A <<'EOF' || true
You are the weave-loop construction crew (phases 1-3). Worktree: WORKTREE_PLACEHOLDER.

1. Read _workspace/HANDOFF.md if present and RESUME; else DISCOVER from TASKS.md M1/M3, seed _workspace/backlog.md, write _workspace/loop_state.md, and stop.
2. Pick the top uncompleted backlog item (first `- [ ]` in _workspace/backlog.md).
3. Run the weave-orchestrator phases 1-3 for this item:
   - Phase 1 (planner): write _workspace/01_planner_plan.md
   - Phase 2 (implementer): edit src/, mirror Store changes across both backends, confirm both `cargo build` and `cargo build --no-default-features --features libsql` compile. Write _workspace/02_implementer_changes.md.
   - Phase 3 (verifier): add matching test layers, run the full gate on BOTH backends (fmt, clippy -D warnings, test). Write _workspace/03_verifier_report.md with GREEN or RED.
4. STOP before Phase 4 (Guardian). Do NOT commit. The diff must remain uncommitted.
5. If verifier is RED, do not proceed. Write the failures to _workspace/03_verifier_report.md and stop this iteration.
6. If verifier is GREEN, write a one-line summary of the diff to _workspace/03_verifier_report.md and stop.
EOF

# ---------------------------------------------------------------------------
# Phase B prompt — MiniMax guardian: review + approve
# ---------------------------------------------------------------------------
read -r -d '' PROMPT_PHASE_B <<'EOF' || true
You are the weave-guardian (Phase 4). You are MiniMax, the external review and approval authority for the weave-loop.

Worktree: WORKTREE_PLACEHOLDER.

Inputs:
- The uncommitted diff in src/ and tests/
- _workspace/01_planner_plan.md
- _workspace/02_implementer_changes.md
- _workspace/03_verifier_report.md (must be GREEN)

Your job:
1. Read the diff, the plan, the change log, and the verifier report.
2. Audit against weave-invariants:
   - No shell (argv-only spawning)
   - Parameterized SQL (bound params! only)
   - Layer DAG intact (no upward deps)
   - Paste-safe injection (exact argv tests)
   - Input caps enforced (MAX_IDENT_LEN, MAX_BODY, MAX_INJECT_CHARS, id_valid)
   - Destructive ops gated (confirm)
   - MCP stdout discipline
   - No new heavyweight default dependency
3. Run the weave-drift-guard scan (check for non-Rust build intrusions).
4. Check docs sync (CHANGELOG.md [Unreleased], README.md, ARCHITECTURE.md if surface changed).

Output:
Write _workspace/04_guardian_review.md with exactly this structure:

```
# Guardian Review
## Invariants
- <file:line> <rule> <PASS/BLOCK>
...

## Drift
- <file> <category> <PASS/BLOCK>
...

## Docs
- <doc> <PASS/BLOCK>
...

## Verdict
APPROVE
```

or

```
## Verdict
BLOCK

## Findings
- <file:line> <specific finding>
...
```

Be strict. A single invariant violation or unaddressed drift is a BLOCK.
EOF

# ---------------------------------------------------------------------------
# Phase C prompt — delivery: commit + PR + auto-merge
# ---------------------------------------------------------------------------
read -r -d '' PROMPT_PHASE_C <<'EOF' || true
You are the weave-loop delivery crew (Phase 5-6). Worktree: WORKTREE_PLACEHOLDER.

1. Read _workspace/04_guardian_review.md. If it does not contain APPROVE, STOP.
2. If APPROVE:
   a. Stage and commit with Conventional Commits subject: `weave: WL-NNN <one-line summary>`.
      Include updated _workspace/backlog.md (flip item to `- [x]`) and _workspace/loop_state.md.
   b. Push the branch: `git push origin HEAD`.
   c. Open a PR: `gh pr create --fill` (or equivalent).
   d. Enable auto-merge: `gh pr merge --auto`.
   e. Update _workspace/loop_state.md: bump cycles_this_session and cycles_total.
   f. If backlog has more items, write _workspace/HANDOFF.md (spawn continuity-steward pattern) for the next session.
   g. If backlog is complete, write _workspace/DONE with evidence.
3. Stop. Do not ScheduleWakeup.
EOF

cd "$WORKTREE"
i=0
while :; do
  i=$((i+1))
  [ "$i" -gt "$MAX_ITERS" ] && { log "MAX_ITERS hit — halting."; exit 3; }
  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }
  [ -f "$WS/DONE" ]        && { log "DONE."; exit 0; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }

  # --- Preflight: ensure a clean slate for this iteration ---
  # Remove stale reports so a prior iteration's GREEN/APPROVE cannot bleed through
  # if the current iteration fails to write new ones.
  rm -f "$WS/03_verifier_report.md" "$WS/04_guardian_review.md"

  # Sanity-check git state: uncommitted changes from a prior crash would confuse Phase A.
  if [ -n "$(git -C "$WORKTREE" status --porcelain 2>/dev/null | grep -v '^??')" ]; then
    log "WARNING: worktree has uncommitted changes. Phase A assumes a clean tree."
    log "         Review manually or stash before continuing."
    sleep "$SLEEP_BETWEEN"
    continue
  fi

  # --- Phase A: plan → implement → verify ---
  log "iter $i/$MAX_ITERS — Phase A: plan→implement→verify"
  PHASE_A_PROMPT="${PROMPT_PHASE_A//WORKTREE_PLACEHOLDER/$WORKTREE}"
  "${AGENT_CMD_ARY[@]}" -p "$PHASE_A_PROMPT" "${AGENT_MODEL_ARGS_ARY[@]}" --add-dir "$WORKTREE" "${APPLY_ARGS[@]}" \
    >>"$WS/ralph-phaseA-$i.log" 2>&1 || log "iter $i Phase A nonzero (continuing from durable state)"

  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }

  if ! grep -qE '^\*\*GREEN\*\*|^GREEN$' "$WS/03_verifier_report.md" 2>/dev/null; then
    log "iter $i — verifier RED or missing. Will retry on next iteration."
    sleep "$SLEEP_BETWEEN"
    continue
  fi

  # --- Phase B: MiniMax guardian (review + approve) ---
  if [ "${WEAVE_SKIP_GUARDIAN:-0}" = "1" ]; then
    log "iter $i — Phase B: SKIP (WEAVE_SKIP_GUARDIAN=1)"
    echo -e '# Guardian Review\n\n## Verdict\nAPPROVE\n\n(skipped via WEAVE_SKIP_GUARDIAN)' > "$WS/04_guardian_review.md"
  else
    log "iter $i — Phase B: MiniMax guardian review+approve"
    PHASE_B_PROMPT="${PROMPT_PHASE_B//WORKTREE_PLACEHOLDER/$WORKTREE}"
    "${GUARDIAN_CMD_ARY[@]}" -p "$PHASE_B_PROMPT" "${AGENT_MODEL_ARGS_ARY[@]}" --add-dir "$WORKTREE" "${APPLY_ARGS[@]}" \
      >>"$WS/ralph-phaseB-$i.log" 2>&1 || log "iter $i Phase B nonzero (continuing from durable state)"
  fi

  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }

  if ! grep -qE '^APPROVE$' "$WS/04_guardian_review.md" 2>/dev/null; then
    log "iter $i — guardian BLOCK. Routing findings to implementer on next iteration."
    sleep "$SLEEP_BETWEEN"
    continue
  fi

  # --- Phase C: delivery (commit + push + PR + auto-merge) ---
  log "iter $i — Phase C: APPROVE — delivering (commit + PR + auto-merge)"
  PHASE_C_PROMPT="${PROMPT_PHASE_C//WORKTREE_PLACEHOLDER/$WORKTREE}"
  "${AGENT_CMD_ARY[@]}" -p "$PHASE_C_PROMPT" "${AGENT_MODEL_ARGS_ARY[@]}" --add-dir "$WORKTREE" "${APPLY_ARGS[@]}" \
    >>"$WS/ralph-phaseC-$i.log" 2>&1 || log "iter $i Phase C nonzero (continuing from durable state)"

  [ -f "$WS/DONE" ]        && { log "DONE."; exit 0; }
  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }

  log "iter $i — cycle complete. Next iteration."
  sleep "$SLEEP_BETWEEN"
done
