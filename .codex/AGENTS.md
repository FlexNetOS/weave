# Codex Environment for weave

This file is the repo-local Codex guidance layer. It supplements any parent
`AGENTS.md` files and applies to the whole repository when Codex is launched
from this checkout.

## Codex-owned surfaces

Use these layers for Codex work in this repo:

1. **Instructions / guidance**: this file, parent `AGENTS.md` files, repo docs,
   and Rust verification rules.
2. **Runtime config**: `.codex/config.toml`, project-local agent configs, MCP
   config, sandbox/approval settings, and feature flags.
3. **Slash command surface**: built-in Codex CLI commands only. Custom prompt
   slash commands are deprecated and belong in `~/.codex/prompts`, not repo
   `.codex/commands`.
4. **Skills**: repo skills in `.agents/skills`; user skills in
   `~/.agents/skills`; system skills bundled with Codex.
5. **Plugins / marketplace**: `.agents/plugins/marketplace.json` plus plugin
   bundles under `plugins/`.
6. **Hooks / rules / permissions**: `.codex/hooks.json`, inline `[hooks]`, and
   real Starlark `.rules` files under `.codex/rules/`.
7. **Tools / MCP / subagents / automation**: configured MCP servers, custom
   agents in `.codex/agents/`, Codex exec/SDK surfaces, and repo-local Rust
   verification tools.

`.claude/` is a Claude-facing harness/reference surface. Codex may read it as
source material when a task explicitly depends on it, but `.claude/` is not a
Codex-owned skill, command, hook, config, or rule layer.

## Rust operating contract

weave is a Rust workspace with members `weave-core`, `weave-inject`,
`weave-mcp`, and `weave`. Keep the shippable product one dependency-light Rust
workspace. Agent metadata under `.codex/`, `.agents/`, `.claude/`, and
`handoff/` may exist as inert sidecar state, but must not become build input or
a second source of runtime truth.

Before changing Rust behavior, read the relevant docs:

- `CLAUDE.md` for the repo's established operating contract.
- `ARCHITECTURE.md` for design boundaries.
- `CONTRIBUTING.md` for contribution expectations.
- `docs/TESTING.md` for the test strategy.

## Verification ladder

Use the smallest gate that proves the change, then expand for shared behavior:

- Formatting only: `cargo fmt --all --check`.
- Local Rust edit: focused `cargo test <name>` plus `cargo clippy --all-targets -- -D warnings`.
- Store/backend edit: run the default backend and libSQL backend gates.
- User-facing CLI/MCP/injector edit: add or update black-box tests under
  `tests/`, then run `cargo test --all-targets`.
- Broad or release-bound edit: run the full gate.

Full gate:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build --no-default-features --features libsql
cargo test --no-default-features --features libsql
```

## Repo skills

- `.agents/skills/weave/SKILL.md`: weave codebase conventions.
- `.agents/skills/rust-workflow/SKILL.md`: Rust implementation and verification
  workflow.
- `.agents/skills/rust-review/SKILL.md`: owner-style Rust review workflow.
- `.agents/skills/codex-rust-env/SKILL.md`: maintain this Codex Rust
  environment.
- `.agents/skills/prompt-loop/SKILL.md`: low-token Kimi worker supervision.

## Custom agents

- `explorer`: read-only code and architecture evidence.
- `reviewer`: correctness/security/regression review.
- `docs_researcher`: primary-source documentation verification.
- `rust_verifier`: Rust-focused gate planning and failure triage.

Spawn subagents only when explicitly requested or when the user asks for a
parallel review/research workflow.

## MCP baseline

Treat `.codex/config.toml` as the repo-local Codex baseline. Keep private
credentials and personal MCPs in `~/.codex/config.toml`, not in this repo.
