---
name: weave-drift-guard
description: weave's Rust-native drift guard — the project's critical concern. Detects and remediates drift away from "one dependency-light Rust binary": non-Rust source/build intrusions, ECC/auto-generated artifacts (.codex, .agents, .claude/*.json, handoff/**, .omc, ecc-pushed packages) that try to feed the build, and generated docs/skills that contradict the real codebase. ALWAYS run at session start and during any weave review. Use whenever a new non-.rs file appears in the build path, an ecc/.omc artifact is auto-pushed, or a generated config/skill seems to disagree with the actual code.
---

# weave Rust-native drift guard

weave's core identity is **one dependency-light Rust binary — no Python, no daemon, no foreign runtime in the shippable artifact.** This repo is also wired to external agent tooling (`ecc-tools`, the `.codex/`, `.agents/`, `.claude/` bundles, and the `handoff/` framework) that **auto-generates and auto-pushes** config/package artifacts. Those tools can introduce files in other languages/formats, or generated docs that *contradict* the real code. That is **drift**, and catching it is a stated critical concern of this project (see `CLAUDE.md`).

Run this guard **at every session start** and during any change review.

## What counts as drift (block-worthy)

A file is drift when it would make weave **anything other than a self-contained Rust build**:

1. **A non-Rust build step** — a Makefile/Justfile/script/`build.rs` that shells out to another language toolchain to produce part of the binary, or a new toolchain config (`package.json`, `pyproject.toml`, `go.mod`, etc.) wired into the build.
2. **A non-Rust dependency in the shipped binary** — anything pulling a foreign runtime (Python, Node) or a heavyweight non-default crate tree into the default build. (Date/time stays date-crate-free; `tokio`/`libsql` stay behind the `libsql` feature.)
3. **A foreign source of truth Rust must mirror by hand** — e.g. an `.omc` file or an ecc-pushed package whose logic the Rust code is expected to track manually. Two copies of behavior that can silently diverge is the dangerous pattern.
4. **Misinformation artifacts** — auto-generated docs/skills/config that *contradict* the real codebase and would mislead an agent (the canonical example: the ECC-generated `weave` skill that falsely claimed camelCase filenames, relative imports, `*.test.*` files, and freeform commits — none true of this snake_case, Conventional-Commits, `#[cfg(test)]`/`tests/` repo).

## What is NOT drift (do not block)

Auto-generated agent-config **sidecars that nothing builds against** are acceptable as inert metadata: `.codex/*.toml`, `.agents/**/*.yaml`, `.claude/identity.json`, `.claude/ecc-tools.json`, and the `handoff/**` YAML/JSON schemas/templates. They live beside the code as tooling hints. They become drift *only* if they start feeding the build/runtime or contradict reality (categories above).

## Detection procedure

```bash
# 1. Any non-Rust file that could feed the build? (ignore target/, .git/, and known inert sidecars)
git ls-files | grep -vE '\.(rs|toml|lock|md)$|^LICENSE|^\.gitignore' \
  | grep -vE '^(\.codex/|\.agents/|\.claude/|handoff/|docs/|\.github/)'

# 2. Foreign toolchain manifests anywhere they'd be picked up?
git ls-files | grep -iE '(^|/)(package\.json|pyproject\.toml|go\.mod|Gemfile|build\.gradle|CMakeLists\.txt|Makefile|Justfile)$'

# 3. .omc or ecc-pushed package artifacts?
git ls-files | grep -iE '\.omc$|ecc.*package|/packages?/'

# 4. New top-level dirs since the last known-good state (review anything unexpected)
git ls-files | awk -F/ '{print $1}' | sort -u
```

Then for every hit:

**Verify before alarming.** A suspect file is only drift if it actually feeds the build/runtime. Confirm by checking whether it is referenced by `Cargo.toml`, `build.rs`, `src/`, or `.github/workflows/ci.yml`:

```bash
grep -rn "<suspect-filename-or-path>" Cargo.toml build.rs src/ .github/ 2>/dev/null
```

If nothing references it and it is an inert sidecar → **note it, don't block.** If it feeds the build, or is category 1–4 above → **drift confirmed.**

## Remediation: transform to Rust-native and sync

When drift is confirmed, do not delete blindly and do not let the foreign artifact stay in the build path. **Port the behavior into Rust and sync the whole codebase in one change:**

1. **Identify the behavior** the foreign artifact provides.
2. **Port it into the right `src/` module** behind the existing abstractions — `Store` for persistence, `Mux`/`Target` for injection, the MCP tool table for protocol surface, `config.rs` for settings. No new module unless the layering demands it.
3. **Sync everything in the same change:**
   - `Cargo.toml` (and keep the default build dependency-light; foreign deps go behind a feature flag or are removed).
   - **both** storage backends if the store was involved.
   - the matching **test layers** (see `weave-test-discipline`) on **both** backends.
   - the **docs** — `ARCHITECTURE.md`, `CONTRIBUTING.md`, `docs/TESTING.md`, `CHANGELOG.md`.
4. **Remove or neutralize the foreign artifact** from the build path once its logic is Rust-native, so there is exactly one source of truth.
5. **For misinformation artifacts**, correct or remove the contradicting doc/skill so no agent is misled.

## Reporting

Produce a short verdict: for each suspect, `file → category → feeds-build? → DRIFT/inert → action`. If any DRIFT is found during a change review, it is a **BLOCK** until ported to Rust-native and synced. If found at session start, surface it to the user as a critical concern with the remediation plan before other work proceeds.
