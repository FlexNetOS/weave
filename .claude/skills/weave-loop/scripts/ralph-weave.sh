#!/usr/bin/env bash
# ralph-weave.sh — external Ralph loop for weave-loop.
# Self-restarts weave-loop with a FRESH context each iteration
# (each `claude -p` process is a clean session = the /new effect)
# until a terminal sentinel.
#
# Sourced from ~/Desktop/meta/HARNESS-UPGRADE-KIT.md §8, tailored to weave.
set -euo pipefail

WORKTREE="${WEAVE_WORKTREE:-/home/drdave/Desktop/meta/weave-harness-loop}"
BUDGET="${WEAVE_BUDGET:-3}"
MAX_ITERS="${WEAVE_MAX_ITERS:-50}"
SLEEP_BETWEEN="${WEAVE_SLEEP:-5}"
MODEL="${WEAVE_MODEL:-opus}"

WS="$WORKTREE/_workspace"
mkdir -p "$WS"

log(){ printf '[ralph-weave %s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }

command -v claude >/dev/null || { log "FATAL: claude not on PATH"; exit 1; }
[ -d "$WORKTREE" ]   || { log "FATAL: worktree $WORKTREE not found"; exit 1; }
[ -f "$WORKTREE/Cargo.toml" ] || { log "FATAL: $WORKTREE/Cargo.toml missing — wrong worktree?"; exit 1; }

APPLY_ARGS=()
if [ "${WEAVE_APPLY:-0}" = "1" ]; then
  APPLY_ARGS=(--dangerously-skip-permissions)
  log "APPLY MODE — will modify the live system unattended."
else
  log "SAFE mode (default): destructive applies refused. Set WEAVE_APPLY=1 to act."
fi

read -r -d '' PROMPT <<EOF || true
/weave-loop resume from _workspace/HANDOFF.md (external Ralph runner, fresh context). Worktree: $WORKTREE.

1. If _workspace/HANDOFF.md exists, follow session-relay RESUME from it (authoritative signal); else DISCOVER and build _workspace/backlog.md from TASKS.md M1/M3.
2. Run up to $BUDGET cycles: one item each, dry-run -> apply for destructive steps, VERIFY across the boundary in a FRESH shell (cargo fmt + clippy + test), commit per cycle with subject `weave-loop: WL-NNN <summary>`. Fail-closed; never weaken a guard.
3. Bootstrap hazard: if a cycle mutates weave's own wire/mux (mcp.rs / store.rs / inject.rs / setup.rs), do NOT depend on the live \`weave\` binary for the handoff heartbeat that cycle. Committed HANDOFF.md is the authoritative resume signal.
4. Then write EXACTLY ONE sentinel under _workspace/ and stop (do not ScheduleWakeup):
   - DONE (with evidence in the file: cycles_total, items_closed, cargo fmt/clippy/test exits)
   - NEEDS-HUMAN (reason + captured artifact path; human wall only — not a spin)
   - else HANDOFF.md (spawn continuity-steward, commit, broadcast relay:handoff if safe)
EOF

cd "$WORKTREE"
i=0
while :; do
  i=$((i+1))
  [ "$i" -gt "$MAX_ITERS" ] && { log "MAX_ITERS hit — halting."; exit 3; }
  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }
  [ -f "$WS/DONE" ]        && { log "DONE."; exit 0; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }

  log "iter $i/$MAX_ITERS — spawning fresh agent (budget=$BUDGET, model=$MODEL)"
  # Best-effort: nonzero exit is logged but does not abort the runner — durable
  # state on disk is the truth, not the per-iter exit code.
  claude -p "$PROMPT" --model "$MODEL" --add-dir "$WORKTREE" "${APPLY_ARGS[@]}" \
    >>"$WS/ralph-run-$i.log" 2>&1 || log "iter $i nonzero (continuing from durable state)"

  [ -f "$WS/DONE" ]        && { log "DONE."; exit 0; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }
  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }

  sleep "$SLEEP_BETWEEN"
done
