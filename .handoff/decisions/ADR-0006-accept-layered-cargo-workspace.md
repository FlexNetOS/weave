# ADR-0006 — Accept the layered Cargo workspace as weave architecture

- **Status:** accepted — 2026-06-27 (implements WL-077; closes/replaces WL-043)
- **Plane:** agent-mesh / build architecture
- **Owner:** drdave
- **Scope:** Cargo layout and module ownership for `weave-core`, `weave-inject`, `weave-mcp`, and `weave`.
- **Supersedes/relates:** WL-043 (single-crate collapse), WL-077 (decision refresh), ADR-0003 (token-light multi-surface), ADR-0004 (human surfaces), ADR-0005 (cross-machine push), CLAUDE.md/ARCHITECTURE workspace map.

## Context

WL-043 recorded the WL-001 four-crate split as unsanctioned structural drift and kept a standing mandate to collapse everything back into one crate after meta workspace alignment. That was a correct warning when the split was new and the docs still described a single crate.

The architecture has since changed materially. The repo now has token-light progressive-disclosure MCP (ADR-0003), feature-gated human and web surfaces (ADR-0004 / ADR-0002), cross-machine delivery work (ADR-0005), runner/job orchestration, and CLI/MCP/dashboard/bot surfaces that all reuse the same core types and store contracts. The crate boundaries are no longer just a temporary folder split; they are doing useful work:

- `weave-core` owns model/config/store/session/export/archive/sign/llm policy without depending on I/O-heavy surfaces.
- `weave-inject` keeps native mux command construction pure and testable, below MCP and CLI glue.
- `weave-mcp` isolates token-light MCP, optional HTTP/dashboard routing, and protocol stdout discipline from the CLI binary.
- `weave` wires CLI, setup, hooks, git tagging, harnesses, and operator-facing I/O.

A mechanical collapse would now be high-risk churn: cross-crate imports, feature propagation, black-box test paths, docs, and CI would all move while delivering no new user capability. Worse, it would weaken the compiler-enforced no-upward-dependency boundaries that protect dependency-light and token-light architecture.

## Decision

Accept the current four-crate Cargo workspace as the supported weave architecture. The invariant is **one dependency-light Rust binary**, not **one crate**. The shipped binary remains `target/release/weave`; default features remain lightweight; optional heavyweight surfaces stay feature-gated; and crate boundaries must stay small, layered, and justified.

WL-043 is closed/replaced by this ADR. Do **not** spend a loop cycle collapsing the workspace solely to satisfy the stale single-crate mandate. Future structural work should improve the accepted workspace, not undo it mechanically.

## Guardrails

1. **No new crate by default.** Adding a crate requires an ADR-level reason and must preserve the existing dependency direction.
2. **Layering remains load-bearing:** `weave-core` has no upward deps; `weave-inject` depends only on `weave-core`; `weave-mcp` depends on core/inject; `weave` is the top-level binary/glue.
3. **One binary remains the product.** The workspace must continue to build one operator binary by default; sidecar agent config remains inert and never a build source of truth.
4. **Feature propagation must stay explicit.** Optional dependency trees (`libsql`, `sign`, `llm`, `surfaces`, `obscura`) remain feature-gated and tested at the workspace root.
5. **Token-light and stdout discipline stay in `weave-mcp`.** New capabilities go through the meta-tool catalog or a documented parity decision; they must not bloat the standing MCP surface.
6. **Collapse is not forbidden forever.** A future collapse may be proposed only if it is tied to a concrete user-visible simplification or build-risk reduction, includes a migration plan, and proves parity across the full dual-backend/feature gate. It is no longer the default backlog item.

## Consequences

- The docs stop carrying contradictory language that calls the workspace both current and temporary.
- Backlog priority moves to capability and reliability items (WL-078 onward) instead of a large structural rewrite with negative expected value.
- The `backup/*` tags that were retained as collapse recovery references are no longer part of an active migration plan. They may still be retained under normal repository retention policy, but this ADR does not require a special no-prune rule.
- Reviewers should treat attempted new upward dependencies, default-heavy dependencies, or MCP standing-surface growth as architecture regressions.

## Verification

This is a documentation/architecture decision only; it changes no Rust source, Cargo metadata, generated artifacts, or dependency graph. Verification for WL-077 is therefore:

- `cargo fmt --all --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`
- `cargo clippy --no-default-features --features libsql -- -D warnings`
- `cargo build --no-default-features --features libsql`
- `cargo test --no-default-features --features libsql`

