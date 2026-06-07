# Contributing to weave

Thanks for hacking on weave. It is a Rust workspace with a small, focused
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

Run all four before opening a PR from the workspace root. The default build uses **sqlite** (via default features) and must stay green.

```bash
cargo build                 # debug build
cargo build --release       # -> target/release/weave
cargo test                  # run the unit tests (currently 10/10)
cargo clippy -- -D warnings # lint; the tree is clippy-clean, keep it that way
cargo fmt --all             # format (CI-style check: cargo fmt --all -- --check)
```

If you touch the feature-gated libSQL backend, also build, lint, and test it:

```bash
cargo build  --no-default-features --features libsql
cargo clippy --no-default-features --features libsql --all-targets -- -D warnings
cargo test   --no-default-features --features libsql
```

If you touch the optional `sign` backend (signed cross-store identity), lint and
test that feature on **both** backends too — it composes with each:

```bash
cargo clippy --all-targets --features sign -- -D warnings
cargo test   --features sign
cargo clippy --no-default-features --features "libsql sign" --all-targets -- -D warnings
cargo test   --no-default-features --features "libsql sign"
```

CI runs all of these as separate jobs (`rustfmt`, `clippy`, `test`, `build (libsql
backend)`, `sign`, `libsql + sign`), so the optional crypto path and the libSQL
test suite are gated on every PR — not just locally.

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
- **Crypto conventions (`sign` feature).** A **fingerprint** is the SHA-256 of the
  **raw public key**; trust and revocation match the **full** digest (the truncated
  `SHA256:<16-hex>` form is display-only, never the basis of a trust decision).
  **Never print a secret** — fingerprint/show/list/rotate/revoke/doctor emit only
  public keys, fingerprints, and paths; rotate *moves* the old private key, it does
  not read or print it. **Verification is RNG-free**, so its tests must seed keys
  from **fixed bytes** (never `OsRng`) and stay deterministic across repeat runs.
  **Keys are multi-per-identity** (#7): the `identity_keys` registry holds several
  pubkeys per identity, so `register_key` (and `weave key add`) **appends** rather
  than overwrites, and `verify_pulled_intent` commits a signed intent IFF it verifies
  against **any registered NON-REVOKED key** for the sender (a revoked key is always
  skipped — R1). Keep that "match any non-revoked registered key" rule when touching
  verification; never let a present-but-invalid sig or a revoked-only match commit.
  **Trust and revocation lists stay receiver-local config** (`WEAVE_TRUST` /
  `WEAVE_REVOKED` / `trust` / `revoked`) — the keys live in the store, but the
  trust/revoke *decision* is config, not a store table.
- **Keep modules layered.** `model` has no I/O; `inject` and `store` depend only
  on `model`; `mcp`/`main` sit on top. Don't add upward dependencies.
- **Doc comments** (`//!` module headers, `///` on public items) explain *why*,
  not just *what* — match the existing tone in `src/`.
- **No new heavyweight dependencies** in the default build. Date/time is handled
  without a date crate on purpose; keep it that way. Anything pulling a new
  dependency belongs behind a feature flag so the default static binary stays
  dependency-light — `libsql` (tokio) and `sign` (`ed25519-dalek`/`getrandom`) are
  the precedents: each crate is `optional` and gated, so a default `cargo tree`
  shows neither. New optional deps should follow the same pattern (and, where it
  matters, a test asserting absence from the default shippable graph).
- Prefer pure, unit-testable functions (like `commands_for`) over functions that
  both compute and perform I/O.
- **Serialize env in unit tests.** A unit test that reads or writes a `WEAVE_*`
  (or any process-global) env var MUST acquire `crate::testenv::lock_env()` and
  mutate via `EnvVarGuard` — the test runner is multithreaded, so unguarded
  `set_var`/`remove_var` on `WEAVE_*` races. Integration/security/prop tests are
  exempt (separate process, scrubbed env). See [docs/TESTING.md](docs/TESTING.md) §1.

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
