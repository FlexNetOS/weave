# Verifier Report — WL-044b (libsql feature trim)

**Worktree:** `/home/drdave/Desktop/meta/weave-wl044b`
**Branch:** `wl-044b-libsql-trim`
**Base:** `origin/develop` (tip `493f2ee`)
**Date:** 2026-06-14
**Toolchain:** cargo-deny 0.19.8

## Change under test (no Rust source touched)
- `weave-core/Cargo.toml`: `libsql = { version = "0.9.30", default-features = false, features = ["core","remote","tls"], optional = true }` (was default features). Drops the unused `replication`/`sync` trees.
- `deny.toml`: dropped the bincode `RUSTSEC-2025-0141` ignore (now eliminated structurally); 5 ignores remain (4 rustls-webpki + RUSTSEC-2025-0134 rustls-pemfile). `all-features = true` retained → gate has teeth on every feature's tree.
- Docs: `CHANGELOG.md`, `docs/SECURITY.md`, `.handoff/loop/backlog.md`, `Cargo.lock`.
- Verified diff matches the described change; **zero Rust source files changed.**

## Per-combination gate results

| # | Command | Result | Notes |
|---|---------|--------|-------|
| 1 | `cargo build --no-default-features --features libsql` | **GREEN** | exit 0 |
| 2 | `cargo clippy --no-default-features --features libsql --all-targets -- -D warnings` | **GREEN** | no issues |
| 3 | `cargo test --no-default-features --features libsql` | **GREEN** | **668 passed, 1 ignored, 0 failed** — matches pre-trim baseline exactly |
| 4a | `cargo build --no-default-features --features "libsql sign"` | **GREEN** | exit 0 |
| 4b | `cargo test --no-default-features --features "libsql sign"` | **GREEN** | 708 passed, 1 ignored, 0 failed |
| 5a | `cargo clippy --no-default-features --features "libsql surfaces" --all-targets -- -D warnings` | **GREEN** | no issues |
| 5b | `cargo build --no-default-features --features "libsql surfaces"` | **GREEN** | exit 0 |
| 6a | `cargo build` (default sqlite) | **GREEN** | exit 0 — libsql is optional, unaffected |
| 6b | `cargo clippy --all-targets -- -D warnings` (default) | **GREEN** | no issues |
| 6c | `cargo fmt --all --check` | **GREEN** | exit 0 |

### libsql test count breakdown (combination 3)
43 + 197 + 6 + 84 + 257 + 57 + 24 = **668 passed**; 1 ignored (in the 198-test binary: 197 passed + 1 ignored). **No capability loss — exact match to the 668/1 baseline.**

## Advisory / supply-chain checks

| Check | Result |
|-------|--------|
| `cargo deny check advisories` | **"advisories ok", exit 0** |
| `advisory-not-detected` warnings | **ZERO** — all 5 remaining ignores are present in the graph (no orphaned ignores) |
| Remaining ignores | 5: `RUSTSEC-2026-0098/0099/0049/0104` (rustls-webpki) + `RUSTSEC-2025-0134` (rustls-pemfile). bincode `RUSTSEC-2025-0141` removed from list. |
| `cargo tree --no-default-features --features libsql -p weave -i bincode` | **"package ID specification `bincode` did not match any packages"** — bincode is STRUCTURALLY GONE from the libsql graph, not merely ignored. RUSTSEC-2025-0141 eliminated. |

### Negative test (deny gate still has teeth)
Dropped `RUSTSEC-2026-0098` from `deny.toml` → `cargo deny check advisories`:
```
error[vulnerability]: Name constraints for URI names were incorrectly accepted
    ├ ID: RUSTSEC-2026-0098
    ├ Advisory: https://rustsec.org/advisories/RUSTSEC-2026-0098
```
**Exit 1**, correct vulnerability error for the exact dropped id. **deny.toml restored** to the WL-044b state (5 ignores, bincode absent); worktree confirmed back to the original uncommitted-WL-044b state (same 6 modified files, nothing left over).

## Cross-boundary / drift checks
- **No new clippy warning vs develop:** all clippy runs pass under `-D warnings` (any warning = hard fail); no Rust source changed, so no new lints possible in weave's own code. Confirmed.
- **deny.toml ↔ dependency graph:** zero `advisory-not-detected` → every ignored id is still reachable; bincode removed from ignore list AND from graph (consistent, no orphan).
- **Default sqlite build unaffected:** libsql is `optional`; build/clippy/fmt all green.

## Overall status: **GREEN**

All 10 gate combinations pass. libsql test count holds at 668/1 (no lost capability). bincode RUSTSEC-2025-0141 eliminated from the graph. Advisory gate clean with zero orphaned ignores; negative test confirms the gate still fails on a real vulnerability. No commit/push/PR performed — leader owns delivery.
