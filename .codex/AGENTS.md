# ECC for Codex CLI

This supplements the root `AGENTS.md` with a repo-local ECC baseline.

## Repo Skill

- Repo-generated Codex skill: `.agents/skills/weave/SKILL.md`
- Forge-loop skill: `.agents/skills/forge-loop/SKILL.md`; the Rust front door is `weave harness forge-loop`
- Claude-facing harness: `.claude/skills/weave-orchestrator/SKILL.md` (entry point) + `weave-invariants`, `weave-test-discipline`, `weave-drift-guard`, with agents in `.claude/agents/weave-*.md`
- Keep user-specific credentials and private MCPs in `~/.codex/config.toml`, not in this repo.

## MCP Baseline

Treat `.codex/config.toml` as the default ECC-safe baseline for work in this repository.
The generated baseline enables GitHub, Context7, Exa, Memory, Playwright, and Sequential Thinking.

## Multi-Agent Support

- Explorer: read-only evidence gathering
- Implementer: single-writer code changes for approved forge-loop tasks
- Verifier: fresh-shell gate runner and regression coverage
- Reviewer/Guardian: correctness, security, drift, and docs review
- Docs researcher: API and release-note verification

## Workflow Files

- `/forge-loop` is installed into the user Codex home by `weave codex-tools install`; it delegates to `weave harness forge-loop`.
- `weave codex-tools doctor` checks the Codex CLI, repo assets, forge-loop skill, and prompt shim.

Use the Rust CLI as the source of truth; prompt shims are convenience wrappers only.