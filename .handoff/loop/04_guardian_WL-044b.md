# Guardian Review — WL-044b (libsql feature trim)

**Worktree:** `/home/drdave/Desktop/meta/weave-wl044b`
**Branch:** `wl-044b-libsql-trim` (off `origin/develop`, tip `493f2ee`)
**Input:** `.handoff/loop/03_verifier_WL-044b.md` — **GREEN** (10/10 combos)
**Date:** 2026-06-14
**Scope:** 6 files, **zero Rust source** (`git diff --name-only | grep '\.rs$'` → empty, confirmed)

## Section 1 — Invariants

- **OK — No-downgrade / capability preservation (THE key check).** weave's libsql usage is
  exclusively `Builder::new_local` (local file, `core`) and `Builder::new_remote` (remote Turso
  HTTPS, `remote`+`tls`). Grepped all four crates' `src/` for the embedded-replica/sync surface
  (`new_synced`, `new_remote_replica`, `new_local_replica`, `.sync()`, `sync_interval`,
  `read_your_writes`, `EncryptionConfig`, `RemoteReplica`, `SyncContext`, replication `frames`):
  **none found.** The only `Builder::new_current_thread` hits are tokio's runtime builder
  (unrelated). The dropped `replication`/`sync` features back exactly those unused APIs, so the
  trim removes **no** capability. Evidence corroborated: libsql test count holds at **668/1**,
  byte-identical to the pre-trim baseline. `weave-core/src/store_libsql.rs:528/536` and the doc
  comment at `:13`/`:16`/`:1200` describe only local+remote. (Owner directive: this is an
  **upgrade**, not a downgrade.)
- **OK — No shell / parameterized SQL / layer DAG / paste-safe / input caps / destructive gating /
  MCP stdout.** No Rust source touched; none of these surfaces can regress. Confirmed vacuously.
- **OK — Advisory-gate integrity.** `cargo deny check advisories` → **"advisories ok"** (re-run by
  guardian, exit 0). bincode `RUSTSEC-2025-0141` removed from **both** the dependency graph
  (`name = "bincode"` absent from `Cargo.lock`; `cargo tree -i bincode` → "did not match any
  packages") **and** the deny.toml ignore list — consistent, no orphan ignore, no silently
  suppressed live advisory. The 5 remaining ignores stay reachable (verifier: zero
  `advisory-not-detected`). `[graph] all-features = true` retained (`deny.toml:21/27`) → the gate
  keeps teeth across every feature tree. Negative test (verifier) drops a real id → exit 1.
- **OK — Remaining residual is genuinely upstream-blocked.** rustls-webpki (RUSTSEC-2026-0098/-0099/
  -0049/-0104) + rustls-pemfile (RUSTSEC-2025-0134) live in the `tls` feature weave needs for remote
  HTTPS; libsql pins `hyper-rustls 0.25` → `rustls 0.22` → `rustls-webpki 0.102` even on git `main`
  (claim checked, documented). Not weave-fixable today; honestly tracked.

## Section 2 — Rust-native drift

- **OK — REDUCES the dependency tree.** Still one dependency-light Rust binary; the change *removes*
  `bincode`, `tonic`, `tonic-web`, `tower-http`, `libsql_replication` from the libsql build (verified
  `tower-http` is unreachable in the libsql graph via `cargo tree -i`; the residual `tower-http` line
  in `Cargo.lock` is an inert, unbuilt lock entry, not drift). No new dependency, no non-Rust build
  step, no non-Rust runtime artifact. `deny.toml` is CI-only supply-chain tooling, not a build input
  to the binary. No misinformation drift: the four docs match the code.

## Section 3 — Docs sync (no fork)

- **OK — All surfaces tell ONE story.** `docs/SECURITY.md §5`, `CHANGELOG.md` (WL-044b *Changed*),
  `.handoff/loop/backlog.md` (WL-044b → `[~]` partial, residual tracked), and the `deny.toml`
  comments are mutually consistent: trim to `core/remote/tls`, bincode eliminated structurally,
  6→5 advisories, residual webpki upstream-pinned (hyper-rustls 0.25, checked on git main), 668/1
  unchanged. No contradiction between the generated artifact (deny.toml) and the Rust reality.

## Owner-directive alignment

This is the **right call**. "Never downgrade / always upgrade / complete the work, carry forward"
is satisfied: fewer deps, one fewer live advisory, lighter build, **zero capability loss**
(668/1 exact), and the unavoidable residual is honestly carried forward as upstream-blocked rather
than hidden. No concrete better option exists today — clearing the webpki line requires a libsql
release on `rustls 0.23`/`hyper-rustls 0.27`, which upstream has not shipped (verified on git main).
Marking WL-044b `[~]` partial with the residual tracked is the correct, non-overclaiming state.

## Verdict: **APPROVE**

No BLOCK findings. No WARN findings. Cleared to deliver: open the PR into `develop` and arm
`gh pr merge <n> --auto --squash`. Leader owns delivery (guardian did not commit/push/PR).
