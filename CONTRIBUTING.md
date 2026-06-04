# Contributing to weave

Thanks for hacking on weave. It is a single Rust binary with a small, focused
surface — the goal is to keep it dependency-light, well-tested, and easy to read.
Please read [ARCHITECTURE.md](ARCHITECTURE.md) first; it explains the module
layout, the `Store` trait, and the injector design you will be extending.

## Prerequisites

- Rust (project tracks current stable; developed on **1.96**). Install via
  [rustup](https://rustup.rs/).
- `cargo` on your `PATH` (e.g. `export PATH="$HOME/.cargo/bin:$PATH"`).

No system libraries are required: SQLite is vendored via `rusqlite`'s `bundled`
feature.

## Build, test, lint, format

Run all four before opening a PR. The default build uses **no features** and must
stay green.

```bash
cargo build                 # debug build
cargo build --release       # -> target/release/weave
cargo test                  # run the unit tests (currently 10/10)
cargo clippy -- -D warnings # lint; the tree is clippy-clean, keep it that way
cargo fmt --all             # format (CI-style check: cargo fmt --all -- --check)
```

If you touch the feature-gated libSQL backend, also build it:

```bash
cargo build --no-default-features --features libsql
cargo clippy --no-default-features --features libsql -- -D warnings
```

A change is ready when the default build is clean, clippy is warning-free, the
formatter reports no diff, and all tests pass.

## Code style

- **Formatting** is whatever `cargo fmt` produces — do not hand-format.
- **Clippy-clean**: no `#[allow(...)]` without a one-line comment justifying it.
- **No shell.** Spawn external programs with `std::process::Command::new(bin)`
  and an explicit argv vector. Never build a command string or call `sh -c`; user
  text (message bodies, session names) must never reach a shell. This is a
  security invariant (see ARCHITECTURE §7), not a preference.
- **Parameterize SQL.** Use bound `params!` for every variable value. The only
  inlined SQL literals are compile-time broadcast constants.
- **Keep modules layered.** `model` has no I/O; `inject` and `store` depend only
  on `model`; `mcp`/`main` sit on top. Don't add upward dependencies.
- **Doc comments** (`//!` module headers, `///` on public items) explain *why*,
  not just *what* — match the existing tone in `src/`.
- **No new heavyweight dependencies** in the default build. Date/time is handled
  without a date crate on purpose; keep it that way. Anything pulling tokio or a
  large tree belongs behind a feature flag (as libSQL is).
- Prefer pure, unit-testable functions (like `commands_for`) over functions that
  both compute and perform I/O.

## Adding a new mux adapter

The injector is designed for this. To support a new multiplexer/terminal (say,
`foomux`), edit `src/inject.rs` only and add three things, then tests:

1. **`commands_for` arm** — add a `Mux::Foomux` variant to the enum and its
   `as_str`, `parse`, and `binary` arms, then a match arm in `commands_for`
   returning the exact argv vector(s) that (a) type the literal text and (b)
   submit it **paste-safely**. Study the existing arms: most TUIs (Claude Code)
   run in bracketed-paste mode, so a bare Enter can be swallowed or read as a
   cancel. Use the terminal's documented idiom — e.g. tmux closes bracketed
   paste with the hex `ESC[201~` sequence before `Enter`; wezterm uses
   `--no-paste`; others append a carriage return (`\r`, i.e. byte 13).

2. **`detect_target` probe** — add a branch that recognizes the environment
   variable the terminal sets for the current pane/window/session (e.g.
   `FOOMUX_PANE`). Order matters: more-specific multiplexers are probed first, so
   place the new branch where it won't shadow or be shadowed incorrectly (a true
   multiplexer that owns the input line should generally precede a bare terminal).

3. **Tests** — add a `commands_for` test asserting the **exact** argv for the new
   mux (copy the shape of `tmux_is_paste_safe`, `zellij_writes_cr`, etc.), and
   extend `binaries_map` to cover `binary()`/`parse()` for the new variant.
   `commands_for` is pure, so these tests run with no multiplexer installed.

That's it — `inject()`, the peer registry, the MCP send path, and the CLI all
work through the `Mux`/`Target` abstraction, so no other code needs to change.
Update the injector table in `README.md` and `ARCHITECTURE.md` if the new adapter
is user-facing.

## Where things live

| Want to change… | Edit |
|---|---|
| Message/Peer shape, timestamp/broadcast helpers | `src/model.rs` |
| Schema, queries, a new `Store` method/backend | `src/store.rs` (or `src/store_libsql.rs`) |
| A mux adapter or submission idiom | `src/inject.rs` |
| An MCP tool or its JSON schema | `src/mcp.rs` |
| A CLI subcommand or hook behavior | `src/main.rs` |
| Config keys / env overlay | `src/config.rs` |

If you change a `Store` method signature, update every backend that implements
the trait so the default and `--features libsql` builds both compile.

**Prefer growing the trait additively.** When a `Store` method needs more data,
add a new method (e.g. `register_peer_full` alongside `register_peer`) rather than
changing an existing method's arity. Keep the old method as a trait *default* that
forwards to the new one with sensible defaults, so every existing call site and
test compiles untouched while both backends gain the richer path. This is the
standard pattern for future `Store` signature growth.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <summary>
```

Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`,
`build`, `ci`. Scopes mirror the modules (`inject`, `store`, `mcp`, `cli`,
`config`, `model`). Examples:

```
feat(inject): add wezterm adapter with --no-paste submission
fix(store): exclude self-sends from broadcast unread count
docs(architecture): document the no-daemon push model
test(inject): assert exact argv for the screen adapter
```

Keep the summary in the imperative mood and under ~72 chars. Put rationale and
any breaking-change note in the body. Update `CHANGELOG.md` under `[Unreleased]`
for user-facing changes.

## Pull requests

- One logical change per PR; keep diffs reviewable.
- Run build + test + clippy + fmt locally first.
- Note any new dependency and why it isn't avoidable (default build stays
  dependency-light).
- Update the relevant docs (README / ARCHITECTURE / CHANGELOG) in the same PR.
