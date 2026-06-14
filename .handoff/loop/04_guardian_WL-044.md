# 04 — Guardian review — WL-044 (Dependabot/RustSec advisory resolution)

**Worktree:** `/home/drdave/Desktop/meta/weave-wl044` (branch `wl-044-dependabot`, base `origin/develop`)
**Diff scope (confirmed):** `.github/workflows/ci.yml`, `.handoff/loop/backlog.md`, `CHANGELOG.md`, `docs/SECURITY.md`, new `deny.toml`. NO Rust source, NO `Cargo.toml`/`Cargo.lock`. Source tree byte-identical to verified-green develop tip.

## VERDICT: BLOCK (one correctable BLOCK; everything else OK)

The change is sound in spirit and almost entirely correct, but the **CI gate as wired does not actually exercise the libsql advisory graph** — so the central documented promise ("any unlisted advisory fails this job; a newly-introduced vuln is caught going forward") is **false for the libsql tree**, which is the only tree the entire `deny.toml` ignore list governs. This is a fixable one-line CI change, routed to the implementer.

---

## §1 — Invariants — OK
Source tree unchanged vs verified-green develop tip; no-shell / parameterized-SQL / layer-DAG / paste-safe / input-caps / destructive-gating / MCP-stdout invariants are all untouched. Nothing to audit at the source level. **OK.**

## §2 — Rust-native drift guard — OK (the headline check passes)
- `deny.toml` is referenced by NO `Cargo.toml` / `build.rs` / `src/` — pure sidecar CI config. Confirmed by grep. **OK.**
- `EmbarkStudios/cargo-deny-action@v2` + `cargo deny` appear in NO `Cargo.toml` — CI-only Rust tooling, not a build or runtime dependency of the shippable binary. **OK.**
- `Cargo.lock` and all manifests unchanged — no new crate, no new dep, no non-Rust build step. **OK.**
- cargo-deny is itself Rust tooling; this *strengthens* the Rust-native posture (adds a supply-chain gate). **Not drift. OK.**

## §2b — Soundness of the security claims — mostly TRUE, verified against the live graph
- **Default build advisory-clean:** `cargo tree -i rustls-webpki` (default features) → `did not match any packages`. **TRUE.**
- **All advisories confined to libsql:** `cargo tree --no-default-features --features libsql -i rustls-webpki` →
  `rustls-webpki 0.102.8 → rustls 0.22.4 → hyper-rustls 0.25.0 → libsql v0.9.30 → weave-core`. `bincode 1.3.3`, `rustls-pemfile 2.2.0` also present only under that tree. **TRUE.**
- **Upstream-pin:** `libsql 0.9.30` pulls `hyper-rustls 0.25 → rustls 0.22 → rustls-webpki 0.102`; the patched `>=0.103` needs rustls 0.23. Inverse tree confirms the exact chain. **TRUE.**
- **`cargo deny check advisories` passes (exit 0):** confirmed, "advisories ok". **TRUE** — but see the BLOCK below for *why* it passes.

## §2c — BLOCK: the CI `audit` job scans the DEFAULT graph, where the libsql advisories are absent, so the ignore list is DORMANT and the "caught going forward" promise is false for the libsql tree

Evidence (run in this worktree, cargo-deny 0.19.8):

1. `cargo deny check advisories` (exactly what CI runs — `command: check advisories`, no feature args) emits **6 `warning[advisory-not-detected]`** for the rustls-webpki/bincode/rustls-pemfile ids: *"no crate matched advisory criteria."* The vuln crates are **not in the default-feature graph**, so the ignores never fire.
2. **Negative test under the DEFAULT graph:** removing `RUSTSEC-2026-0098` from the ignore list → still `advisories ok`, **exit 0**. A new/unlisted libsql-tree vuln would NOT fail this job.
3. **Negative test under `--no-default-features --features libsql`:** removing the same id → `error[vulnerability]: RUSTSEC-2026-0098 … security vulnerability detected`, **exit 1**. The gate only has teeth when the libsql feature is resolved.

Consequence: the job correctly gates the **default shippable binary's** tree (good — a future default-tree vuln WOULD fail it), but it provides **zero** ongoing coverage of the libsql remote-TLS stack — the exact stack `deny.toml`'s entire ignore list documents and that WL-044b is meant to track to closure. The ignores are inert in CI; WL-044b's removal trigger ("the gate will flag them as advisory-not-detected") is *already* firing on the default run today, for the wrong reason (absence, not upstream-fix), which would mask the real signal.

This contradicts three load-bearing claims in the change (a docs/CI ↔ reality fork):
- **deny.toml:** "any advisory NOT explicitly listed here fails `cargo deny check advisories`."
- **ci.yml comment:** "Any advisory NOT listed there fails this job — so a newly-introduced vuln is caught going forward."
- **SECURITY.md §5:** "the gate fails on any advisory **not** in that explicit list."

All three are TRUE for the default graph and FALSE for the libsql graph the ignores describe.

### Prescribed remediation (route to weave-implementer)
Make the CI `audit` job resolve the **libsql** feature graph (and ideally the default graph too), so the gate actually exercises the crates the ignore list governs. Concretely, pass the feature flags to cargo-deny in the action, e.g.:

```yaml
- uses: EmbarkStudios/cargo-deny-action@v2
  with:
    command: check advisories
    arguments: --no-default-features --features libsql
```

(Or two steps / a matrix: one default, one `--features libsql sign`, to cover both the shippable-binary tree and the optional-feature tree.) Re-verify after the fix:
- `cargo deny --no-default-features --features libsql check advisories` → exit 0, and **no** `advisory-not-detected` for the six ids (they should be *detected-and-ignored*, not *not-detected*).
- Negative test under that invocation: removing any ignore id → the job fails (exit 1).

Then reconcile WL-044b's removal trigger in deny.toml/SECURITY.md/backlog to "detected-and-ignored → goes away once libsql adopts rustls 0.23," since under the corrected (feature-resolved) gate the ids WILL be detected (not `advisory-not-detected`) until upstream fixes them.

## §3 — Docs sync — OK except the coverage-overstatement folded into §2c
deny.toml ↔ SECURITY.md §5 ↔ CHANGELOG Security ↔ backlog WL-044/WL-044b tell a consistent story on advisory ids, the upstream-block reason (rustls 0.23 / hyper-rustls 0.27 vs libsql's ^0.25), and the WL-044b trigger. The only inconsistency is the "gate catches it going forward" claim vs the default-graph reality (BLOCK §2c) — fix the CI job and the docs become true as written. Minor (WARN, non-blocking): SECURITY.md/CHANGELOG label `rustls-pemfile` only as "unmaintained" while the live version is `2.2.0`; the table doesn't assert a version so it's not wrong — leave as-is.

## §4 — Owner-directive alignment — OK
"Never downgrade / always upgrade / carry forward incomplete work, don't remove": this change ADDS a gate that did not exist, PRESERVES the libsql remote-Turso capability (no downgrade), and DOCUMENTS + TRACKS (WL-044b) rather than silently ignoring. Accepting a documented, scoped, upstream-pinned exception is the correct call here — the alternative (dropping libsql remote TLS, or force-pinning rustls 0.23 which the resolver rejects) would be a removal/capability loss. I have **no objection to the exception itself**; the BLOCK is purely that the gate must actually scan the graph it claims to govern, or it is decorative for the libsql tree.

---

## Routing
- **BLOCK → weave-implementer:** one-line CI fix (feature-resolve the `audit` job for libsql, re-verify the negative test, reconcile the WL-044b trigger wording). Re-review after fix (Part 2 drift re-scan is cheap and will re-run).
- Everything else is OK / verified-true. Once the gate resolves the libsql graph, this is a clean **APPROVE**.

---

# RE-REVIEW (post-fix) — WL-044 — VERDICT: APPROVE

**Date:** 2026-06-14 · **Worktree:** `/home/drdave/Desktop/meta/weave-wl044` (branch `wl-044-dependabot`)
**Fix applied since the BLOCK:** `deny.toml` now sets `[graph] all-features = true` (UNION graph) and the
ci.yml `audit` job runs `command: check advisories` with **no feature args** (config-driven scope). The prior
BLOCK (§2c) is **RESOLVED**: the gate now exercises the libsql remote-TLS tree it governs.

## Independently re-run negative test (cargo-deny 0.19.8, this worktree) — PASS
1. **Baseline:** `cargo deny check advisories` → `advisories ok`, **exit 0**.
2. **advisory-not-detected count = 0** — all 6 ids are DETECTED-and-ignored (proves the libsql tree IS in
   scope under `all-features = true`; the dormant-ignore defect from §2c is gone).
3. **NEGATIVE TEST (re-run by guardian):** dropped `RUSTSEC-2026-0098` from the ignore list →
   `error[vulnerability]: … RUSTSEC-2026-0098 … security vulnerability detected`, **exit 1**,
   `advisories FAILED`. The inverse tree in the error confirms the in-scope chain
   `rustls-webpki 0.102.8 → rustls 0.22.4 → hyper-rustls 0.25.0 → libsql 0.9.30 → weave-core`.
   **The gate now has teeth on the libsql graph.**
4. **Restored** deny.toml (6 ids) → `advisories ok`, **exit 0**. Clean.

## §1 — Invariants — OK
Source tree byte-identical to verified-green develop tip (no Rust source in the diff). No-shell /
parameterized-SQL / layer-DAG / paste-safe / input-caps / destructive-gating / MCP-stdout: all untouched.

## §2 — Rust-native drift — OK
- `deny.toml` and `cargo-deny`/`cargo deny` are referenced by NO `Cargo.toml`/`build.rs`/`src` (git grep
  empty) — pure CI-only Rust tooling; feeds nothing into the binary build/runtime. **Not drift.**
- `Cargo.toml` / `Cargo.lock` **untouched** vs develop (`git diff --name-only -- *Cargo.toml *Cargo.lock`
  empty) — no dep added, no source change, no non-Rust build step. cargo-deny is itself Rust tooling and
  *strengthens* the Rust-native supply-chain posture.

## §3 — Docs sync — OK (the §2c fork is closed; all claims now TRUE for the libsql graph)
- **ci.yml audit-job comment:** explains `[graph] all-features = true` is REQUIRED so the scan sees the
  libsql crates, and states "Any advisory NOT in that explicit list fails this job — proven by the negative
  test." **TRUE** (re-verified above). No stale `--all-features` CLI arg (cargo-deny rejects args after
  `check`) — the job is purely config-driven, matching the approach. **Correct.**
- **deny.toml header / `[graph]` comment:** "any advisory NOT explicitly listed here fails
  `cargo deny check advisories`" + the all-features rationale (metadata-only resolution, sqlite+libsql
  coexist). **TRUE.**
- **SECURITY.md §5 (Dependency advisories):** "the gate fails on any advisory **not** in that explicit
  list." **TRUE** for the libsql graph now (was the false claim in §2c). Advisory table + upstream-pin
  rationale + WL-044b removal trigger consistent.
- **WL-044b removal trigger (backlog/deny.toml):** "delete the now-stale ids … the gate will flag them as
  `advisory-not-detected`." **NOW TRUE under all-features:** today the 6 ids are detected-and-ignored
  (0 not-detected, proven); they become `advisory-not-detected` only AFTER libsql adopts rustls 0.23 and
  the patched crates enter the graph — exactly the deletion signal. The §2c "firing today for the wrong
  reason (absence)" defect is gone. CHANGELOG `[Unreleased]` Security entry matches.

## §4 — Scope — OK
Diff = `.github/workflows/ci.yml`, `deny.toml` (untracked, new), `docs/SECURITY.md`, `CHANGELOG.md`,
`.handoff/loop/backlog.md`. Nothing else. `Cargo.toml`/`Cargo.lock` untouched.

## VERDICT: **APPROVE**
The prior single BLOCK (dormant ignore list / decorative gate on the libsql tree) is fully remediated and
independently re-verified by the guardian's own negative test. Invariants clean, no Rust-native drift,
docs↔CI↔config fork closed. Clear to deliver the PR into `develop`.
