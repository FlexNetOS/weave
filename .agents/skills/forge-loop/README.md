# Weave Forge Loop

The forge loop is Weave's Rust-native Codex execution engine. It is not just a
prompt, a slash command, or a shell wrapper. The durable control plane lives in
the compiled Weave CLI, with this skill as the workflow contract and an optional
Codex `/forge-loop` prompt shim as convenience UI.

## Quick start

```bash
weave harness forge-loop --task "close the next task"   # dry-run the plan
weave harness forge-loop --execute --task "..."         # delegate one cycle to codex exec
weave codex-tools doctor                                # check Codex CLI/assets/shim
weave codex-tools install                               # install the user /forge-loop shim
```

Dry-run is the default. The dry-run prints the selected task, the seven execution
layers, the `codex exec` command shape, and the exact `WEAVE_FORGE_*`
environment. Use `--json` when another orchestrator needs machine-readable plan
output.

## What the forge loop is

At the center is:

```bash
weave harness forge-loop --task "..."
```

With execution enabled:

```bash
weave harness forge-loop --execute --task "..."
```

The command delegates one bounded cycle to `codex exec`, but the durable source
of truth remains repository-owned:

```text
.agents/skills/forge-loop/SKILL.md
```

The optional Codex slash command `/forge-loop` is only a shim installed by:

```bash
weave codex-tools install
```

That layering is intentional: the Rust CLI is the engine, the skill is the
workflow specification, and `/forge-loop` is a thin user-facing shortcut.

## Execution model

The loop runs one cohesive task cycle through seven layers:

1. **Recover durable state** — inspect git, `.handoff`, ICM, and existing Codex
   context before starting work.
2. **Pick one cohesive task** — prevent scope explosion and keep diffs
   reviewable.
3. **Use subagents where they help** — allow read-heavy exploration and review in
   parallel, while keeping implementation single-writer.
4. **Implement through Rust-native Weave** — use the CLI and workspace as the
   control plane, not ad hoc scripts or prompt-only state.
5. **Verify in fresh shells** — run fmt, clippy, tests, and the relevant
   backend/feature gates for the touched area.
6. **Deliver immediately** — commit, push, open a PR to `develop`, and arm
   auto-merge when green.
7. **Persist state or halt cleanly** — write memory/handoff state, finish on
   `DONE`, or stop with a committed `NEEDS-HUMAN` sentinel for real external
   blockers.

## Why this is the right execution engine for Weave

The forge loop is optimized for Weave's actual constraints, not for a generic
agent demo.

### Rust is the execution authority

Weave's invariant is a dependency-light Rust binary with no foreign runtime as
the source of truth. For that reason, `/forge-loop` is deliberately not the real
system. Prompt shims can drift, shell scripts can sprawl, and generated config can
mislead agents. The engine is compiled, tested, versioned, reviewed, and shipped
with Weave.

### Control plane and convenience UI are separate

The surfaces have distinct jobs:

- `weave harness forge-loop` — real execution engine.
- `.agents/skills/forge-loop/SKILL.md` — durable workflow instructions.
- `/forge-loop` — optional convenience shim in the user's Codex home.

If Codex prompt conventions change, the engine survives. If the user shim is
missing, the Rust command still works.

### Dry-run comes first

Autonomous loops should be inspectable before they act. The default mode prints
what will run instead of running it. Operators can review the task, command
shape, environment, and delivery contract before passing `--execute`.

### It follows the modern Codex shape

The loop uses Codex where Codex is strongest:

- `codex exec` for non-interactive automation.
- Skills for durable, reusable workflows.
- Subagents for bounded read-heavy exploration and review.
- Project `.codex` files as inert repo sidecars.
- User-level prompt shims only for user-level convenience.

That keeps Weave aligned with Codex's current architecture without making a
transient prompt format the system of record.

### It encodes the delivery rule

The loop bakes in the expected delivery behavior: every completed chunk is
committed, pushed, opened as a PR, and auto-merge is armed. This is not left as a
chat reminder; it is part of the execution contract.

### It avoids multi-agent write chaos

Many multi-agent systems fail because several agents edit the tree at the same
time. The forge loop is stricter:

- Parallelism is for exploration, documentation research, and review.
- Implementation stays single-writer.
- Verification and guardian review gate the result.

This is faster in practice because it avoids conflicting patches, contradictory
edits, and noisy context pollution.

### It composes with the existing harness

The forge loop does not replace the older `ide-merge-ide` lane. It adds a
Codex-native front door beside it:

```bash
weave harness forge-loop       # Codex-native task execution
weave harness ide-merge-ide    # Kimi/Ollama/Claude MiniMax Ralph loop
```

That makes the change a strict upgrade: no removal, no downgrade, and no broken
legacy path.

## Operator contract

Use the forge loop when you want one bounded Weave task cycle with durable state,
fresh verification, and immediate PR delivery.

Use dry-run first:

```bash
weave harness forge-loop --task "describe the next task" --json
```

Execute only when ready:

```bash
weave harness forge-loop --execute --task "describe the next task"
```

Run the Codex tool doctor when the slash command or Codex integration is suspect:

```bash
weave codex-tools doctor
```

Install or refresh the user shim when needed:

```bash
weave codex-tools install --weave-exe /path/to/stable/weave
```

## Short version

The forge loop is a compiled Rust orchestration surface that uses Codex as an
execution worker, not as the source of truth. It is Rust-native, dry-run-first,
subagent-aware, single-writer, PR-delivery oriented, compatible with existing
harnesses, and resilient to prompt/config drift.
