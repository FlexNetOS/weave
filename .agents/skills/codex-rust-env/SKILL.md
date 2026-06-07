---
name: codex-rust-env
description: Maintain the repo-local Codex Rust environment for weave across AGENTS guidance, .codex config, rules, hooks, repo skills, plugins, custom agents, MCP, and verification automation.
metadata:
  owner: weave
  type: codex-environment
---

# codex-rust-env

Use this skill when changing Codex-owned environment surfaces for this Rust
workspace.

## Codex-owned layers

- Guidance: `.codex/AGENTS.md` and parent `AGENTS.md` files.
- Runtime config: `.codex/config.toml` and `.codex/agents/*.toml`.
- Skills: `.agents/skills/*/SKILL.md`.
- Plugins: `.agents/plugins/marketplace.json` and `plugins/*/.codex-plugin/plugin.json`.
- Rules: `.codex/rules/*.rules`.
- Hooks: `.codex/hooks.json` and scripts under `.codex/hooks/`.

Do not treat `.claude/` as Codex-owned. It may be a reference source only when
the task explicitly needs Claude harness material.

## Maintenance rules

1. Keep repo dotfiles tracked.
2. Do not put secrets, provider overrides, or personal MCP auth in project
   config.
3. Prefer `bunx` over `npx` for local JavaScript MCP launchers unless CI or a
   repo policy requires otherwise.
4. Add YAML frontmatter with `name` and `description` to every Codex skill.
5. Use real Starlark `.rules` files for command policy; Markdown policy docs do
   not enforce permissions.
6. Keep hooks lightweight and non-destructive. New or changed hooks require
   Codex trust review before they run.
