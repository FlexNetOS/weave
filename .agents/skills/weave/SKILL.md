# weave Development Patterns

Repo-specific conventions for the **weave** codebase — a single dependency-light **Rust** binary (agent-to-agent session mesh + native multi-mux terminal injector). The authoritative source of truth is `CLAUDE.md` and the `.claude/skills/weave-*` harness skills; this file mirrors the essentials for the Codex/OpenAI-agents bundle.

> Note: an earlier auto-generated version of this file was inaccurate. The conventions below are verified against the actual codebase.

## Coding conventions (actual)

- **File naming:** snake_case Rust modules — `model.rs`, `store.rs`, `store_libsql.rs`, `inject.rs`, `mcp.rs`, `config.rs`, `setup.rs`, `main.rs`. (Not camelCase.)
- **Imports:** `crate::`/`super::` paths and std; the crate is one binary with focused modules.
- **Module layering (acyclic):** `model` (no I/O) ← `inject`/`store`/`config` ← `mcp`/`setup` ← `main`. Never add an upward dependency.
- **Tests:** in-module `#[cfg(test)]` unit tests **plus** black-box suites in `tests/` (`integration.rs`, `security.rs`, `prop.rs`) and a criterion bench in `benches/`. There are no `*.test.*` files.
- **Commits:** Conventional Commits — `feat(inject): …`, `fix(store): …`, `docs(...)`, `test(...)`. Scopes mirror modules. (Not freeform.)

## Non-negotiable invariants

No shell (argv-only `Command::new(bin).args(...)`, never `sh -c`); parameterized SQL (`params!`, the only inline literals are the `BROADCAST`-derived aliases); paste-safe injection (`commands_for` is pure; close bracketed paste before Enter); input caps (`MAX_IDENT_LEN`, `MAX_BODY`=65536, `MAX_INJECT_CHARS`=240, `id_valid`); destructive ops `confirm`-gated; MCP writes only protocol frames to stdout, logs to stderr; dependency-light default build (foreign/heavy deps behind a feature flag, as `libsql` is).

## The verification gate (dual backend)

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test  --all-targets                              # default sqlite backend
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build  --no-default-features --features libsql   # mutually-exclusive backend
cargo test   --no-default-features --features libsql
```

A store-touching change is done only when **both** backends are green.

## Rust-native drift

weave must stay one self-contained Rust build. ECC/auto-generated artifacts (this file included, `.codex/`, `.claude/*.json`, `handoff/**`, any `.omc` or ecc-pushed package) are acceptable only as inert sidecars — never feeding the build, never a foreign source of truth Rust mirrors by hand, never contradicting the code. See `.claude/skills/weave-drift-guard/SKILL.md` for the full guard.
