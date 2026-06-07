# Verifier report — WL-001 workspace split

## Scope

Independent verification of the mechanical workspace split produced by the
implementer. Added one new test layer: `mock_injector_implements_trait` in
`weave-mcp/src/mcp.rs` to prove the new `Injector` trait abstraction accepts a
mock implementation.

## Verification commands run

All commands executed from `/home/drdave/Desktop/meta/weave` in a fresh shell.

### Format + clippy (sqlite default)

```
cargo fmt --all -- --check                -> exit 0
cargo clippy --all-targets -- -D warnings -> exit 0
```

### Tests (sqlite default)

```
cargo test --all-targets -> exit 0
```

| crate / suite | passed | failed | ignored |
|---------------|--------|--------|---------|
| weave (bin unit) | 25 | 0 | 0 |
| weave/tests::integration | 112 | 0 | 0 |
| weave/tests::prop | 4 | 0 | 0 |
| weave/tests::security | 45 | 0 | 0 |
| weave-core unit | 178 | 0 | 0 |
| weave-inject unit | 31 | 0 | 0 |
| weave-mcp unit | 2 | 0 | 0 |

### libsql backend

```
cargo build --no-default-features --features libsql -> exit 0
cargo clippy --no-default-features --features libsql --all-targets -- -D warnings -> exit 0
cargo test --no-default-features --features libsql --all-targets -> exit 0
```

| crate / suite | passed | failed | ignored |
|---------------|--------|--------|---------|
| weave (bin unit) | 24 | 0 | 0 |
| weave/tests::integration | 112 | 0 | 1 |
| weave/tests::prop | 4 | 0 | 0 |
| weave/tests::security | 45 | 0 | 0 |
| weave-core unit | 153 | 0 | 0 |
| weave-inject unit | 31 | 0 | 0 |
| weave-mcp unit | 2 | 0 | 0 |

### Optional `sign` feature

```
cargo test --features sign --all-targets -> exit 0
cargo test --no-default-features --features "libsql sign" --all-targets -> exit 0
```

Both sign builds green; sign-specific suites expand from 112/45 to 128/58.

## Fixes applied during verify

1. **Added `default = ["sqlite"]` and `sqlite = ["weave-core/sqlite"]` to
   `weave-mcp/Cargo.toml`.** Without it, `cargo test -p weave-mcp` selected no
   backend and the federation imports were unresolved.

2. **Added `default-features = false` to the `weave-mcp` dependency in
   `weave/Cargo.toml`.** Without it, the libsql build pulled in `weave-mcp`'s
   default `sqlite` feature, causing both backends to be active and duplicate
   `federated_*` / `pull_from_store` definitions in `weave-core`.

3. **Commented out `default = ["sqlite"]` in `weave-core/Cargo.toml`.** The
   workspace split means `weave-core` is always consumed with an explicit backend
   selected by the bin or MCP crate; keeping a default here caused the libsql
   build to enable both backends when member crates were resolved independently.

4. **Added `mock_injector_implements_trait` test** in
   `weave-mcp/src/mcp.rs` to satisfy the planner's Injector-conformance test
   layer.

## Cross-boundary checks

- `Store` trait and both `SqliteStore` / `LibsqlStore` remain in `weave-core`;
  schema + migration methods were moved together and are mirrored.
- `Injector` trait lives in `weave-inject`; `weave-mcp::serve<I: Injector>` is
  generic; binary provides `RealInjector`; test proves mock compiles and
  coerces to `&dyn Injector`.
- Layer DAG preserved: `weave-core` has no I/O deps; `weave-inject` depends only
  on `weave-core`; `weave-mcp` depends on `weave-core` + `weave-inject`;
  `weave` bin wires all three.
- Binary name preserved: `target/debug/weave` is produced; integration tests
  resolve via `CARGO_BIN_EXE_weave` unchanged.

## Docs sync check

- `ARCHITECTURE.md` updated with workspace map.
- `CONTRIBUTING.md` updated with workspace build instructions.
- `docs/TESTING.md` updated with new paths.
- `CHANGELOG.md [Unreleased]` entry added for workspace split.

## Verdict

**GREEN** on both backends (sqlite + libsql) and both sign variants. The diff is
ready for guardian review (Phase 4).
