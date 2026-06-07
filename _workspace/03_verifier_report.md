# Verifier report — WL-002 Phase A (daemon store + CLI)

## Scope

Phase A of WL-002: add `presence` table, Store trait methods (`heartbeat`,
`presence`, `evict_stale_presence`, `peer_liveness`), CLI `weave daemon`
subcommands, and matching tests. MCP tools deferred to Phase B.

## Verification commands run

All commands executed from `/home/drdave/Desktop/meta/weave-mcp-daemon-tools`
in a fresh shell.

### Format + clippy

```
cargo fmt --all -- --check                                    -> exit 0
cargo clippy --all-targets -- -D warnings                     -> exit 0
cargo clippy --no-default-features --features libsql --all-targets -- -D warnings -> exit 0
```

### Tests — sqlite (default)

```
cargo test --all-targets -> exit 0
```

| crate / suite | passed | failed | ignored | notes |
|---------------|--------|--------|---------|-------|
| weave (bin unit) | 25 | 0 | 0 | |
| weave/tests::integration | 113 | 0 | 0 | +1 daemon lifecycle |
| weave/tests::prop | 4 | 0 | 0 | |
| weave/tests::security | 45 | 0 | 0 | |
| weave-core unit | 181 | 0 | 0 | +3 presence tests |
| weave-inject unit | 31 | 0 | 0 | |
| weave-mcp unit | 2 | 0 | 0 | |

### Tests — libsql backend

```
cargo test --no-default-features --features libsql --all-targets -> exit 0
```

| crate / suite | passed | failed | ignored | notes |
|---------------|--------|--------|---------|-------|
| weave (bin unit) | 24 | 0 | 0 | |
| weave/tests::integration | 113 | 0 | 1 | |
| weave/tests::prop | 4 | 0 | 0 | |
| weave/tests::security | 45 | 0 | 0 | |
| weave-core unit | 156 | 0 | 0 | +3 presence tests |
| weave-inject unit | 31 | 0 | 0 | |
| weave-mcp unit | 2 | 0 | 0 | |

### Optional `sign` feature

```
cargo test --features sign --all-targets                      -> exit 0
cargo test --no-default-features --features "libsql sign" --all-targets -> exit 0
```

Both sign builds green; weave-core grows from 196→199 (sqlite+sign) and
164→167 (libsql+sign) due to the three new presence unit tests.

## Cross-boundary checks

- Schema migration is **additive** (`CREATE TABLE IF NOT EXISTS`) and guarded
  by `PRAGMA user_version` bump in both backends.
- `Store` trait changes mirrored in `SqliteStore` and `LibsqlStore`.
- `peer_liveness` is a default trait method — no backend-specific logic needed.
- `daemon_running` uses argv-only `kill -0`; no shell.
- `handle_daemon` uses `Stdio::null()` for detached spawn — no TTY leak.
- Integration test uses `WEAVE_PIDFILE` env override for parallel safety.

## Docs sync check

- `CHANGELOG.md` has `[Unreleased] — presence daemon (v0.2, WL-002 Phase A)`
  entry.
- `README.md` / `ARCHITECTURE.md` daemon section explicitly deferred to Phase B.

## Verdict

**GREEN** on both backends (sqlite + libsql) and both sign variants.
Phase A is complete and ready for guardian review. Phase B (MCP tools) remains
for the next cycle.
