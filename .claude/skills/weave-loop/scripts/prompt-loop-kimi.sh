#!/usr/bin/env bash
# prompt-loop-kimi.sh — detached Kimi worker plus low-token Codex monitor.
#
# Primary command:
#   bash .claude/skills/weave-loop/scripts/prompt-loop-kimi.sh resume kimi-cli codex-min
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"

usage() {
  cat <<'EOF'
Usage:
  prompt-loop-kimi.sh resume kimi-cli codex-min [--dry-run] [--worktree PATH]
  prompt-loop-kimi.sh status [--worktree PATH] [--tail N]
  prompt-loop-kimi.sh monitor [--worktree PATH] [--tail N]

Environment:
  WEAVE_WORKTREE       Override worktree path.
  WEAVE_HANDOFF        Override handoff path relative to worktree.
  WEAVE_KIMI_BIN       Override Kimi binary (default: kimi, then kimi-cli).
  WEAVE_KIMI_EXTRA_ARGS Extra arguments inserted before -p.
EOF
}

die() {
  printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2
  exit 1
}

utc_now() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

json_escape() {
  # Compact escape for one-line status values.
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/	/\\t/g'
}

resolve_handoff_worktree() {
  local handoff_path="$1"
  [ -f "$handoff_path" ] || return 1
  awk -F: '
    /^(resume\.)?worktree:/ {
      sub(/^[ \t]+/, "", $2)
      sub(/[ \t]+$/, "", $2)
      print $2
      exit
    }
  ' "$handoff_path"
}

resolve_worktree() {
  local explicit="$1"
  if [ -n "$explicit" ]; then
    printf '%s\n' "$explicit"
    return
  fi
  if [ -n "${WEAVE_WORKTREE:-}" ]; then
    printf '%s\n' "$WEAVE_WORKTREE"
    return
  fi
  local handoff="${WEAVE_HANDOFF:-_workspace/HANDOFF.md}"
  local from_handoff
  from_handoff="$(resolve_handoff_worktree "$PWD/$handoff" || true)"
  if [ -n "$from_handoff" ]; then
    printf '%s\n' "$from_handoff"
    return
  fi
  printf '%s\n' "$PWD"
}

resolve_kimi_bin() {
  if [ -n "${WEAVE_KIMI_BIN:-}" ]; then
    command -v "$WEAVE_KIMI_BIN" >/dev/null || die "WEAVE_KIMI_BIN not found: $WEAVE_KIMI_BIN"
    printf '%s\n' "$WEAVE_KIMI_BIN"
    return
  fi
  if command -v kimi >/dev/null; then
    printf '%s\n' kimi
    return
  fi
  if command -v kimi-cli >/dev/null; then
    printf '%s\n' kimi-cli
    return
  fi
  die "neither kimi nor kimi-cli is on PATH"
}

pid_alive() {
  local pid="$1"
  [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

write_status() {
  local ws="$1"
  local state="$2"
  local worktree="$3"
  local branch="$4"
  local pid="$5"
  local message="$6"
  local status="$ws/agent_status.json"
  local tmp="$status.tmp.$$"
  cat >"$tmp" <<EOF
{
  "schema": "weave.prompt-loop.kimi-status.v1",
  "state": "$(json_escape "$state")",
  "worker": "kimi-cli",
  "profile": "codex-min",
  "worktree": "$(json_escape "$worktree")",
  "branch": "$(json_escape "$branch")",
  "pid": "$(json_escape "$pid")",
  "last_heartbeat_utc": "$(utc_now)",
  "current_item": "",
  "last_gate": "",
  "needs_human": false,
  "message": "$(json_escape "$message")"
}
EOF
  mv "$tmp" "$status"
}

build_prompt() {
  local worktree="$1"
  local status="$2"
  local log="$3"
  local handoff="${WEAVE_HANDOFF:-_workspace/HANDOFF.md}"
  cat <<EOF
You are Kimi Code running the weave prompt-loop worker in YOLO/APPLY mode.

Worktree: $worktree
Entry point: /weave-loop resume from $handoff
Supervisor profile: codex-min

Hard requirements:
1. cd to the worktree above before reading or editing files.
2. Treat $handoff as the authoritative resume signal. If it names another worktree, switch there and update $status with the resolved path.
3. Do the loop work autonomously. Do not ask the user for ordinary approvals.
4. Retry transient failures such as DNS, network, rate-limit, and temporary remote errors. Only genuine walls write _workspace/NEEDS-HUMAN and stop: sudo, interactive auth, hardware, or branch protection requiring human review.
5. Keep Codex token burn low. Do not rely on a human or Codex watching your terminal. Write durable state to files.
6. Use rtk-prefixed shell commands in this repository when running shell commands.

Status contract:
- Before and after each phase, rewrite $status as valid JSON.
- Update last_heartbeat_utc before long commands and immediately after they finish.
- Include state as one of: starting, running, verifying, delivering, blocked, done.
- Include current_item, last_gate, needs_human, and a one-line message.
- Keep messages short. Put large output in _workspace artifacts, not stdout.

Log contract:
- Full Kimi output is already redirected by the launcher to $log.
- Do not print large diffs or full test logs. Write summaries to _workspace/*.md.

Loop contract:
- Resume the weave-loop from $handoff.
- Run _workspace/verify-on-resume.sh before claiming a resumed baseline is safe.
- Pick the next backlog item and complete one cohesive cycle at a time.
- Update _workspace/backlog.md, _workspace/loop_state.md, and _workspace/HANDOFF.md.
- At cycle budget, hand off to a fresh autonomous session through committed HANDOFF.md.
- On success, set $status state to done with a concise evidence message.
- On a true human wall, write _workspace/NEEDS-HUMAN and set $status state to blocked.
EOF
}

status_snapshot() {
  local worktree="$1"
  local tail_lines="$2"
  local ws="$worktree/_workspace"
  local pid_file="$ws/kimi-cli.pid"
  local status="$ws/agent_status.json"
  local log="$ws/kimi-cli.log"
  local needs_human="$ws/NEEDS-HUMAN"

  printf 'worktree: %s\n' "$worktree"
  if [ -f "$pid_file" ]; then
    local pid
    pid="$(cat "$pid_file")"
    if pid_alive "$pid"; then
      printf 'pid: %s (running)\n' "$pid"
    else
      printf 'pid: %s (exited)\n' "$pid"
    fi
  else
    printf 'pid: none\n'
  fi

  if [ -s "$status" ]; then
    printf '\nagent_status.json:\n'
    sed -n '1,80p' "$status"
  else
    printf '\nagent_status.json: missing or empty\n'
  fi

  if [ -s "$needs_human" ]; then
    printf '\nNEEDS-HUMAN:\n'
    sed -n '1,80p' "$needs_human"
  fi

  if [ "$tail_lines" -gt 0 ] && [ -f "$log" ]; then
    printf '\n%s tail -n %s:\n' "$log" "$tail_lines"
    tail -n "$tail_lines" "$log"
  fi
}

cmd_resume() {
  local agent="${1:-}"
  local profile="${2:-}"
  shift 2 || true
  [ "$agent" = "kimi-cli" ] || [ "$agent" = "kimi" ] || die "resume currently supports only kimi-cli"
  [ "$profile" = "codex-min" ] || die "resume currently supports only codex-min"

  local dry_run=0
  local explicit_worktree=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --dry-run)
        dry_run=1
        shift
        ;;
      --worktree)
        [ "$#" -ge 2 ] || die "--worktree requires a path"
        explicit_worktree="$2"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown resume argument: $1"
        ;;
    esac
  done

  local worktree
  worktree="$(resolve_worktree "$explicit_worktree")"
  [ -d "$worktree" ] || die "worktree not found: $worktree"
  [ -f "$worktree/Cargo.toml" ] || die "Cargo.toml missing in worktree: $worktree"

  local ws="$worktree/_workspace"
  mkdir -p "$ws"
  local pid_file="$ws/kimi-cli.pid"
  local log="$ws/kimi-cli.log"
  local prompt_file="$ws/kimi-cli.prompt.md"
  local status="$ws/agent_status.json"
  local branch
  branch="$(git -C "$worktree" branch --show-current 2>/dev/null || true)"

  if [ -f "$pid_file" ] && pid_alive "$(cat "$pid_file")"; then
    write_status "$ws" "running" "$worktree" "$branch" "$(cat "$pid_file")" "Kimi worker already running."
    status_snapshot "$worktree" 0
    exit 0
  fi

  build_prompt "$worktree" "$status" "$log" >"$prompt_file"
  write_status "$ws" "starting" "$worktree" "$branch" "" "Prompt generated; launching Kimi worker."

  if [ "$dry_run" = "1" ]; then
    write_status "$ws" "done" "$worktree" "$branch" "" "Dry run generated prompt; Kimi was not launched."
    printf 'dry_run: true\nprompt: %s\nstatus: %s\n' "$prompt_file" "$status"
    exit 0
  fi

  local kimi_bin
  kimi_bin="$(resolve_kimi_bin)"
  local -a extra_args=()
  if [ -n "${WEAVE_KIMI_EXTRA_ARGS:-}" ]; then
    # shellcheck disable=SC2206
    extra_args=($WEAVE_KIMI_EXTRA_ARGS)
  fi
  local prompt
  prompt="$(cat "$prompt_file")"

  (
    cd "$worktree"
    nohup "$kimi_bin" -y --output-format stream-json "${extra_args[@]}" -p "$prompt" >"$log" 2>&1 &
    printf '%s\n' "$!" >"$pid_file"
  )

  local pid
  pid="$(cat "$pid_file")"
  write_status "$ws" "running" "$worktree" "$branch" "$pid" "Kimi worker launched detached; supervise via agent_status.json."
  status_snapshot "$worktree" 0
}

cmd_status() {
  local explicit_worktree=""
  local tail_lines=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --worktree)
        [ "$#" -ge 2 ] || die "--worktree requires a path"
        explicit_worktree="$2"
        shift 2
        ;;
      --tail)
        [ "$#" -ge 2 ] || die "--tail requires a line count"
        tail_lines="$2"
        case "$tail_lines" in
          ''|*[!0-9]*) die "--tail must be a non-negative integer" ;;
        esac
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown status argument: $1"
        ;;
    esac
  done
  local worktree
  worktree="$(resolve_worktree "$explicit_worktree")"
  status_snapshot "$worktree" "$tail_lines"
}

main() {
  local cmd="${1:-}"
  case "$cmd" in
    resume)
      shift
      [ "$#" -ge 2 ] || die "resume requires: kimi-cli codex-min"
      cmd_resume "$@"
      ;;
    status|monitor)
      shift
      cmd_status "$@"
      ;;
    -h|--help|'')
      usage
      ;;
    *)
      die "unknown command: $cmd"
      ;;
  esac
}

main "$@"
