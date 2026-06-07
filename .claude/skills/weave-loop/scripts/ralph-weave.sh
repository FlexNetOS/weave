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
MODEL="${WEAVE_MODEL:-minimax-m3:cloud}"
AGENT_CMD="${WEAVE_AGENT_CMD:-ollama launch claude --model minimax-m3:cloud --}"
AGENT_MODEL_ARGS="${WEAVE_AGENT_MODEL_ARGS:-}"
KIMI_PLAN="${WEAVE_KIMI_PLAN:-1}"
KIMI_REVIEW="${WEAVE_KIMI_REVIEW:-1}"
KIMI_CMD="${WEAVE_KIMI_CMD:-kimi-legacy}"
KIMI_MODEL="${WEAVE_KIMI_MODEL:-kimi-code/kimi-for-coding}"
KIMI_SESSION="${WEAVE_KIMI_SESSION:-3c6e42cf-090d-4553-a84b-e63fb9c511c1}"
KIMI_SESSION_FLAG="${WEAVE_KIMI_SESSION_FLAG:--r}"
KIMI_EXTRA_ARGS="${WEAVE_KIMI_EXTRA_ARGS:---quiet}"

WS="$WORKTREE/_workspace"
mkdir -p "$WS"

log(){ printf '[ralph-weave %s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }

read -r -a AGENT_CMD_ARY <<<"$AGENT_CMD"
read -r -a AGENT_MODEL_ARGS_ARY <<<"$AGENT_MODEL_ARGS"
read -r -a KIMI_EXTRA_ARGS_ARY <<<"$KIMI_EXTRA_ARGS"
command -v "${AGENT_CMD_ARY[0]}" >/dev/null || { log "FATAL: ${AGENT_CMD_ARY[0]} not on PATH"; exit 1; }
[ -d "$WORKTREE" ]   || { log "FATAL: worktree $WORKTREE not found"; exit 1; }
[ -f "$WORKTREE/Cargo.toml" ] || { log "FATAL: $WORKTREE/Cargo.toml missing — wrong worktree?"; exit 1; }

if [ -z "$KIMI_SESSION_FLAG" ] && command -v "$KIMI_CMD" >/dev/null; then
  if "$KIMI_CMD" --help 2>/dev/null | grep -q -- '-r,'; then
    KIMI_SESSION_FLAG="-r"
  else
    KIMI_SESSION_FLAG="-S"
  fi
fi

run_kimi_code() {
  local prompt="$1"
  local out="$2"
  local err="$3"
  local args=()

  [ -n "$KIMI_SESSION" ] && args+=("$KIMI_SESSION_FLAG" "$KIMI_SESSION")
  [ -n "$KIMI_MODEL" ] && args+=("-m" "$KIMI_MODEL")
  "$KIMI_CMD" "${args[@]}" "${KIMI_EXTRA_ARGS_ARY[@]}" -p "$prompt" >"$out" 2>"$err"
}

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
2. Run up to $BUDGET cycles: one item each, dry-run -> apply for destructive steps, VERIFY across the boundary in a FRESH shell (cargo fmt + clippy + test), commit per cycle with subject 'weave-loop: WL-NNN <summary>'. Fail-closed; never weaken a guard.
3. Bootstrap hazard: if a cycle mutates weave's own wire/mux (mcp.rs / store.rs / inject.rs / setup.rs), do NOT depend on the live 'weave' binary for the handoff heartbeat that cycle. Committed HANDOFF.md is the authoritative resume signal.
4. If _workspace/kimi-plan-latest.md or _workspace/kimi-review-latest.md exists, read it before selecting the next backlog item. Treat Kimi Code as a planning/review partner: use concrete risks and verification gaps, but you own the implementation and build result.
5. Then write EXACTLY ONE sentinel under _workspace/ and stop (do not ScheduleWakeup):
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

  if [ "$KIMI_PLAN" = "1" ]; then
    if command -v "$KIMI_CMD" >/dev/null; then
      log "iter $i — running Kimi Code preflight (cmd=$KIMI_CMD, model=$KIMI_MODEL, session=$KIMI_SESSION, flag=$KIMI_SESSION_FLAG)"
      run_kimi_code "You are Kimi Code coordinating with Ollama MiniMax for the weave project build loop in $WORKTREE.

Do not edit files. Read _workspace/backlog.md, _workspace/HANDOFF.md if present, _workspace/kimi-review-latest.md if present, TASKS.md if present, and the current git status.

Return a concise preflight for the next MiniMax implementation pass:
- the next backlog/build item to attempt
- correctness risks MiniMax should handle
- exact verification expected, including cargo fmt, cargo clippy, and cargo test
- anything MiniMax must avoid because of the weave bootstrap hazard" \
        "$WS/kimi-plan-$i.md" "$WS/kimi-plan-$i.err" && cp "$WS/kimi-plan-$i.md" "$WS/kimi-plan-latest.md" || log "iter $i Kimi preflight failed (continuing; see $WS/kimi-plan-$i.err)"
    else
      log "iter $i Kimi preflight skipped: $KIMI_CMD not on PATH"
    fi
  fi

  ITER_PROMPT="$PROMPT"
  if [ -s "$WS/kimi-plan-latest.md" ]; then
    ITER_PROMPT="$ITER_PROMPT

Kimi Code K2.6 preflight for this MiniMax pass:
$(sed -n '1,180p' "$WS/kimi-plan-latest.md")"
  fi
  if [ -s "$WS/kimi-review-latest.md" ]; then
    ITER_PROMPT="$ITER_PROMPT

Kimi Code K2.6 review from the previous pass:
$(sed -n '1,180p' "$WS/kimi-review-latest.md")"
  fi

  log "iter $i/$MAX_ITERS — spawning fresh MiniMax agent (budget=$BUDGET, model=$MODEL, cmd=$AGENT_CMD)"
  # Best-effort: nonzero exit is logged but does not abort the runner — durable
  # state on disk is the truth, not the per-iter exit code.
  "${AGENT_CMD_ARY[@]}" -p "$ITER_PROMPT" "${AGENT_MODEL_ARGS_ARY[@]}" --add-dir "$WORKTREE" "${APPLY_ARGS[@]}" \
    >>"$WS/ralph-run-$i.log" 2>&1 || log "iter $i nonzero (continuing from durable state)"

  if [ "$KIMI_REVIEW" = "1" ]; then
    if command -v "$KIMI_CMD" >/dev/null; then
      log "iter $i — running Kimi Code review (cmd=$KIMI_CMD, model=$KIMI_MODEL, session=$KIMI_SESSION, flag=$KIMI_SESSION_FLAG)"
      run_kimi_code "Review the completed MiniMax weave-loop iteration in $WORKTREE.

Do not edit files. Inspect _workspace/HANDOFF.md if present, _workspace/ralph-run-$i.log, git status, the latest commit, and any changed files. Report only:
- concrete correctness risks
- missing or weak verification
- whether the weave project build loop should continue, stop DONE, or write NEEDS-HUMAN
- the next action MiniMax should take on the following iteration" \
        "$WS/kimi-review-$i.md" "$WS/kimi-review-$i.err" && cp "$WS/kimi-review-$i.md" "$WS/kimi-review-latest.md" || log "iter $i Kimi review failed (continuing; see $WS/kimi-review-$i.err)"
    else
      log "iter $i Kimi review skipped: $KIMI_CMD not on PATH"
    fi
  fi

  [ -f "$WS/DONE" ]        && { log "DONE."; exit 0; }
  [ -f "$WS/NEEDS-HUMAN" ] && { log "NEEDS-HUMAN: $(cat "$WS/NEEDS-HUMAN")"; exit 2; }
  [ -f "$WS/STOP" ]        && { log "STOP — halting."; exit 2; }

  sleep "$SLEEP_BETWEEN"
done
