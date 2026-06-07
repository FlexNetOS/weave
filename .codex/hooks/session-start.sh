#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

printf '[codex-weave] rust workspace: '
if [ -f Cargo.toml ]; then
  printf 'Cargo.toml present; '
else
  printf 'Cargo.toml missing; '
fi

if [ -d .agents/skills ]; then
  count="$(find .agents/skills -mindepth 2 -maxdepth 2 -name SKILL.md 2>/dev/null | wc -l | tr -d ' ')"
  printf 'repo skills=%s; ' "$count"
else
  printf 'repo skills=0; '
fi

if [ -d .codex/rules ]; then
  rules="$(find .codex/rules -maxdepth 1 -name '*.rules' 2>/dev/null | wc -l | tr -d ' ')"
  printf 'rules=%s\n' "$rules"
else
  printf 'rules=0\n'
fi
