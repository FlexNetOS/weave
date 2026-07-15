---
name: meta-gitkb-review
description: Review a FlexNetOS Meta peer through the shared Meta/GitKB policy while preserving its independent repository and native assistant adapters.
---

# Meta / GitKB Peer Review

Use this skill when reviewing work in a repository registered by the Meta
control plane. The repository remains independent: do not treat `meta/src` as
a monorepo, copy another peer's rules, or replace its native Codex/Claude
adapter.

## Establish scope

1. From the Meta root, inspect `meta project list --json`. Parent Meta plus
   declared projects with a `path` beginning `src/` are the fleet policy scope.
   Do not discover raw directories below `src/`; worktrees, fixtures, and
   vendor payloads are not policy participants.
2. Use `meta exec --include src -- <command>` for a declared-peer fleet action.
   Audit the parent Meta repository separately.
3. `meta git review` is only a pass-through. Do not use it unless `git review`
   is installed and its behavior has been verified. Use the declared command
   through `meta exec` instead.

## Review workflow

1. Read the repository's `AGENTS.md` and `.kb/AGENTS.md` when present.
2. Start or locate a GitKB task before a non-trivial change. Use current CLI
   forms such as `git kb list --path context/`, `git kb show <slug>`,
   `git kb checkout <slug>`, and `git kb commit -m <message> <pathspec>`.
3. Compare the task criteria with source and tests. Record each criterion as
   done, partial, not done, or needing behavioral verification.
4. Preserve one owner for every MCP server. A repo-local config, a global
   config, and a plugin must not claim the same server name.
5. Treat `.claude/` and `.codex/` as native, repository-local adapters. Add
   this skill only; do not mirror a Claude surface into Codex or overwrite an
   existing adapter to make it look uniform.

## Agent-environment verification

Use the profile-owned Nu and RTK frontdoor for fleet commands:

```nu
~/.nix-profile/toolbin/nu -l -c '^rtk proxy -- envctl agent audit --config agent-env.yaml --scope project --locked'
```

The audit is read-only. It must prove the committed config and lock agree with
the installed skill contents. If it reports an MCP conflict, an untracked
shadow, or a hash mismatch, stop and resolve the owning input; never hand-edit
generated runtime outputs.

## What is not a fleet default

- `agent-env-codex`, `agent-env-config`, `env-stabilize`, and
  `env-toolchain-install` are envctl-specific.
- `codedb-config-tables` is conditional on a verified CodeDB/Yazelix use case.
- Exa or any other MCP is conditional on explicit per-assistant ownership.
- Yazelix alone owns the Nix profile and Nu/RTK runtime. This skill only
  consumes the profile-owned frontdoor.
