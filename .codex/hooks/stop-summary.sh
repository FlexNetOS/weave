#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

status="$(git status --short 2>/dev/null | wc -l | tr -d ' ')"
printf '[codex-weave] stop: changed_paths=%s\n' "$status"
