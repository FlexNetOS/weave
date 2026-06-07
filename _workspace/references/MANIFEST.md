# Reference Repository Manifest

Last full scan: 2026-06-07

Tracked repositories used for continuous feature cross-reference against weave.
Each repo has its own feature-inventory file. The goal is to surface capabilities
from adjacent tools that could upgrade weave.

## Schema

### `features/` — one file per reference repo

Filename: `<owner>--<repo>.md` (slashes replaced with double-dash)

Front-matter:
```yaml
---
repo: "owner/repo"
url: "https://github.com/owner/repo"
language: "rust | python | go | ts | ..."
last_scanned: "YYYY-MM-DD"
scan_agent: "<agent-id>"
status: "active | stale | archived"
---
```

Body sections (mandatory):
1. **Elevator pitch** — one sentence what this tool does.
2. **Feature inventory** — bullet list of ALL capabilities (copy the repo's own docs/CLI).
3. **Weave overlap** — capabilities weave already has (with mapping).
4. **Weave gaps** — capabilities weave does NOT have, ranked by impact (High / Medium / Low).
5. **Proposed WL items** — concrete backlog slugs for each gap.
6. **Integration opportunities** — can weave integrate with (not replace) this tool?
7. **Notes** — architectural tradeoffs, security concerns, non-goals.

### `gaps/` — deduplicated cross-repo gap index

Filename: `GAP-<NNN>.md`

Generated manually or by a script that collates `features/*.md` §4-5.
Each gap links back to the source repo(s) where it was observed.

## Maintenance process

1. **Add a repo** — create a new `features/` file from the template, scan it once.
2. **Re-scan** — update `last_scanned` when a major version drops or when the
   weave-loop resumes and picks a "scan cycle" task.
3. **Harvest gaps** — after every scan, update `gaps/` and the main `backlog.md`.
4. **Archive** — if a repo goes unmaintained or its features stabilize with no
   new gaps for 6+ months, mark `status: archived`.

## Tracked repos

| File | Repo | Language | Status | Last Scanned |
|------|------|----------|--------|--------------|
| `prassanna-ravishankar--repowire.md` | prassanna-ravishankar/repowire | Python | scanned | 2026-06-07 |
| `Dicklesworthstone--mcp_agent_mail.md` | Dicklesworthstone/mcp_agent_mail | Python/Rust | scanned | 2026-06-07 |
| `randlee--atm-core.md` | randlee/atm-core | Rust | scanned | 2026-06-07 |
| `Dicklesworthstone--cross_agent_session_resumer.md` | Dicklesworthstone/cross_agent_session_resumer | Rust | scanned | 2026-06-07 |
| `musistudio--claude-code-router.md` | musistudio/claude-code-router | TypeScript | scanned | 2026-06-07 |
| `numman-ali--cc-mirror.md` | numman-ali/cc-mirror | TypeScript/Shell | scanned | 2026-06-07 |

## Secondary / suggested repos (not yet queued)

| Repo | Why |
|------|-----|
| `anthropics/anthropic-cookbook` | MCP patterns, agent orchestration recipes |
| `cline/cline` | VS Code agent with task execution |
| `aider-ai/aider` | Multi-file editing, git integration, architect mode |
| `codex-cli/codex` | OpenAI's agent CLI — hook patterns, sandboxing |
| `continuedev/continue` | IDE-native agent — context management |
| `supermaven/sm-agent` | Agentic IDE features |
| `e2b-dev/e2b` | Sandboxed code execution — reservation leases |
| `langchain-ai/langchain` | Orchestration primitives |
| `tmux/tmux` | Native multiplexer features — injector parity |
