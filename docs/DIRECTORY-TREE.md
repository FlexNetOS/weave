# Weave directory hierarchy

This is a terminal-native, source-of-truth map of the current `weave` checkout.
It intentionally includes top-level dot folders and the nested handoff/template dot
folders because those directories are part of the agent/control-plane story.

Generated/tool-owned directories are shown, but not recursively expanded when doing
so would only enumerate build cache or Git object internals. In this checkout that
means `.git/` and `target/` are summarized rather than treated as source.

## Legend

```text
[tracked]      committed source/docs/config
[ignored]      intentionally local or generated; ignored by .gitignore
[generated]    tool/build output, reproducible from source or external tools
[sidecar]      agent/handoff metadata; not Rust build input
[runtime]      local state produced by tools while operating the repo
```

## High-level ownership graph

```text
weave/
├── Rust product source
│   ├── weave-core/      model, config, store backends, security primitives
│   ├── weave-inject/    native terminal/mux side effects
│   ├── weave-mcp/       MCP, HTTP/dashboard, Obscura protocol surfaces
│   └── weave/           final binary: CLI, hooks, setup, bridges, tests
│
├── Agent/control sidecars
│   ├── .agents/         OpenAI/Codex skill bundle for this repo
│   ├── .claude/         Claude agents, commands, skills, local settings
│   ├── .codex/          Codex config/agent profiles
│   └── .handoff/        durable planning/ADR/task/loop ledger sidecar
│
├── Docs/release/security
│   ├── docs/
│   ├── README.md
│   ├── ARCHITECTURE.md
│   ├── CHANGELOG.md
│   ├── SECURITY docs via docs/SECURITY.md
│   └── deny.toml
│
└── Tool/generated state
    ├── .git/            Git object/ref/index internals
    └── target/          Cargo build artifacts and cache
```

## Full professional tree

```text
weave/
├── .agents/ [tracked][sidecar]
│   └── skills/
│       └── weave/
│           ├── SKILL.md
│           └── agents/
│               └── openai.yaml
│
├── .claude/ [tracked+ignored][sidecar]
│   ├── agents/
│   │   ├── continuity-steward.md
│   │   ├── weave-guardian.md
│   │   ├── weave-implementer.md
│   │   ├── weave-planner.md
│   │   └── weave-verifier.md
│   ├── commands/
│   │   ├── session-relay.md
│   │   └── weave-loop.md
│   ├── homunculus/
│   │   └── instincts/
│   │       └── inherited/
│   │           └── weave-instincts.yaml
│   ├── skills/
│   │   ├── session-relay/
│   │   │   └── SKILL.md
│   │   ├── weave-drift-guard/
│   │   │   └── SKILL.md
│   │   ├── weave-invariants/
│   │   │   └── SKILL.md
│   │   ├── weave-loop/
│   │   │   ├── SKILL.md
│   │   │   └── scripts/
│   │   │       ├── README.md
│   │   │       └── ralph-weave.sh
│   │   ├── weave-orchestrator/
│   │   │   └── SKILL.md
│   │   └── weave-test-discipline/
│   │       └── SKILL.md
│   ├── ecc-tools.json
│   ├── identity.json
│   ├── scheduled_tasks.lock [ignored][runtime]
│   └── settings.local.json [ignored][runtime]
│
├── .codex/ [tracked][sidecar]
│   ├── agents/
│   │   ├── docs-researcher.toml
│   │   ├── explorer.toml
│   │   └── reviewer.toml
│   ├── AGENTS.md
│   └── config.toml
│
├── .git/ [generated]
│   └── Git internals: objects, refs, index, hooks, logs, config.
│       Not source. Summarize with `git count-objects -vH` rather than expanding.
│
├── .github/ [tracked]
│   └── workflows/
│       ├── ci.yml
│       └── sync-master.yml
│
├── .handoff/ [tracked+ignored][sidecar]
│   ├── context/
│   │   ├── PRD.md
│   │   └── capsule.json
│   ├── decisions/
│   │   ├── ADR-0001-adopt-handoff-kernel.md
│   │   ├── ADR-0002-obscura-web-access-integration.md
│   │   ├── ADR-0003-token-light-multi-surface.md
│   │   ├── ADR-0004-rust-native-human-surfaces.md
│   │   └── ADR-0005-cross-machine-push-delivery.md
│   ├── hooks/
│   │   └── hooks.toml
│   ├── loop/
│   │   ├── _done/
│   │   │   ├── _workspace_prev/
│   │   │   │   ├── references/
│   │   │   │   │   ├── MANIFEST.md
│   │   │   │   │   └── features/
│   │   │   │   │       ├── Dicklesworthstone--cross_agent_session_resumer.md
│   │   │   │   │       ├── Dicklesworthstone--mcp_agent_mail.md
│   │   │   │   │       ├── TEMPLATE.md
│   │   │   │   │       ├── musistudio--claude-code-router.md
│   │   │   │   │       ├── numman-ali--cc-mirror.md
│   │   │   │   │       ├── prassanna-ravishankar--repowire.md
│   │   │   │   │       └── randlee--atm-core.md
│   │   │   │   ├── 01_planner_plan.md
│   │   │   │   ├── HANDOFF.md
│   │   │   │   ├── backlog.md
│   │   │   │   ├── loop_state.md
│   │   │   │   └── verify-on-resume.sh
│   │   │   ├── sessions-handoff/
│   │   │   │   ├── docs/
│   │   │   │   │   ├── sessions-handoff.md
│   │   │   │   │   └── sessions-handoff.pdf
│   │   │   │   ├── examples/
│   │   │   │   │   └── hf-resume-output.json
│   │   │   │   ├── roadmap/
│   │   │   │   │   └── backlog.yaml
│   │   │   │   ├── schemas/
│   │   │   │   │   ├── packet.schema.json
│   │   │   │   │   ├── session.schema.json
│   │   │   │   │   └── task.schema.json
│   │   │   │   ├── templates/
│   │   │   │   │   ├── .handoff/
│   │   │   │   │   │   ├── hooks/
│   │   │   │   │   │   │   └── hooks.toml
│   │   │   │   │   │   ├── policies/
│   │   │   │   │   │   │   └── rules.toml
│   │   │   │   │   │   ├── skills/
│   │   │   │   │   │   │   └── session-resume.skill.md
│   │   │   │   │   │   └── tasks/
│   │   │   │   │   │       └── TASK-0001.task.yaml
│   │   │   │   │   └── AGENTS.md
│   │   │   │   └── manifest.txt
│   │   │   ├── 01_planner_plan.md
│   │   │   └── NEXT_SESSION_PROMPT.md
│   │   ├── backlog.md
│   │   ├── loop_state.md
│   │   ├── TASKS.md
│   │   ├── LESSONS.md
│   │   ├── evaluation.md
│   │   ├── proposed-upgrades.md
│   │   ├── WL-051_changes.md
│   │   ├── WL-052_changes.md
│   │   ├── WL-053_changes.md
│   │   ├── plan_WL-038.md ... plan_WL-042.md
│   │   ├── impl_WL-038.md ... impl_WL-042.md
│   │   ├── 03_verifier_*.md
│   │   └── 04_guardian_*.md
│   ├── packets/
│   │   ├── .gitkeep
│   │   ├── 2026-06-11-reconciliation.md
│   │   └── latest.md
│   ├── policies/
│   │   └── rules.toml
│   ├── tasks/
│   │   └── TASK-0001.task.json
│   ├── HARNESS-CHANGELOG.md
│   ├── README.md
│   ├── ledger.db [ignored][runtime]
│   └── policy.toml
│
├── docs/ [tracked]
│   ├── ARCHITECTURE-GRAPHS.md
│   ├── DIRECTORY-TREE.md
│   ├── FORMAT-session-export.md
│   ├── MULTI-SURFACE-PARITY.md
│   ├── OPERATIONS.md
│   ├── REPOWIRE-PARITY.md
│   ├── ROADMAP-v0.2.md
│   ├── ROADMAP-v0.3.md
│   ├── SECURITY.md
│   ├── SPEC-ask-ack.md
│   ├── SPEC-stop-wake.md
│   └── TESTING.md
│
├── weave-core/ [tracked]
│   ├── Cargo.toml
│   └── src/
│       ├── archive.rs        backup/archive primitives
│       ├── config.rs         config file + env overlay
│       ├── export.rs         HTML mailbox export renderer
│       ├── lib.rs            crate exports
│       ├── llm.rs            optional LLM summarization client
│       ├── memory.rs         mesh memory store
│       ├── model.rs          core types and validation helpers
│       ├── session.rs        session export/import IR
│       ├── sign.rs           optional Ed25519 signing/trust helpers
│       ├── store.rs          default SQLite Store backend
│       ├── store_libsql.rs   optional libSQL/Turso Store backend
│       ├── testenv.rs        test-only environment guards
│       └── webpolicy.rs      governed web/SSRF policy
│
├── weave-inject/ [tracked]
│   ├── Cargo.toml
│   └── src/
│       ├── inject.rs         mux detection, injection, spawn, kill
│       └── lib.rs
│
├── weave-mcp/ [tracked]
│   ├── Cargo.toml
│   └── src/
│       ├── dashboard.rs      dashboard rendering
│       ├── http.rs           HTTP/API/dashboard transport
│       ├── lib.rs
│       ├── mcp.rs            MCP JSON-RPC server + catalog
│       └── obscura.rs        governed Obscura MCP client
│
├── weave/ [tracked]
│   ├── Cargo.toml
│   ├── benches/
│   │   └── weave_bench.rs
│   ├── src/
│   │   ├── backup.rs          backup/restore CLI glue
│   │   ├── git.rs             git tag capture helpers
│   │   ├── harness.rs         Codex/harness orchestration
│   │   ├── main.rs            binary CLI and dispatcher
│   │   ├── provider_switch.rs CC Switch provider bridge
│   │   ├── session.rs         session export/import CLI glue
│   │   ├── setup.rs           setup/uninstall host wiring
│   │   ├── slack.rs           Slack bridge
│   │   ├── telegram.rs        Telegram bridge
│   │   └── testenv.rs
│   └── tests/
│       ├── common/
│       │   └── mod.rs
│       ├── integration.rs
│       ├── prop.rs
│       └── security.rs
│
├── target/ [ignored][generated]
│   ├── CACHEDIR.TAG
│   ├── debug/
│   │   └── Cargo debug/dev/test build artifacts, fingerprints, deps, incremental cache
│   ├── release/
│   │   └── Cargo release build artifacts, fingerprints, deps, incremental cache
│   ├── flycheck0/
│   │   └── editor/checker build artifacts
│   └── tmp/
│
├── .gitignore
├── ARCHITECTURE.md
├── CHANGELOG.md
├── CLAUDE.md
├── CONTRIBUTING.md
├── Cargo.lock
├── Cargo.toml
├── LESSONS-LEARNED.md
├── LICENSE-APACHE
├── LICENSE-MIT
├── README.md
├── cc-switch-main.zip [ignored][local audit evidence]
├── deny.toml
└── rustfmt.toml
```

## Quantitative scan snapshot

```text
tracked files: 167
non-generated files excluding .git/ and target/: 171
all-filesystem directory max depth: 7
all-filesystem file max depth: 8

largest local directories/files:
  target/             21G   [ignored][generated]
  target/debug/       20G   [ignored][generated]
  target/release/     319M  [ignored][generated]
  cc-switch-main.zip  25M   [ignored][local audit evidence]
  .git/               15M   [generated]
  weave/              1.3M  [tracked]
  weave-core/         1.3M  [tracked]
  .handoff/           908K  [sidecar]
```

## Rust source depth

```text
75,301 total Rust lines
  30,139 weave-core/src
  20,174 weave/tests
  13,897 weave/src
   7,942 weave-mcp/src
   2,878 weave-inject/src
```

Largest Rust modules:

```text
14,188 weave/tests/integration.rs
11,249 weave-core/src/store.rs
 8,503 weave/src/main.rs
 8,481 weave-core/src/store_libsql.rs
 6,376 weave-mcp/src/mcp.rs
 5,117 weave/tests/security.rs
 3,909 weave-core/src/config.rs
 3,075 weave-core/src/model.rs
 2,869 weave-inject/src/inject.rs
 2,197 weave/src/setup.rs
```

## Notes for reviewers

- `target/` is listed because it matters operationally, but it is not source.
  It is ignored by `.gitignore` and is Cargo's default build output directory.
- `cc-switch-main.zip` is ignored but intentionally retained locally as supplied
  upstream audit evidence. Do not delete it during normal cleanup.
- `.claude/scheduled_tasks.lock`, `.claude/settings.local.json`, and
  `.handoff/ledger.db` are local runtime files and ignored by Git.
