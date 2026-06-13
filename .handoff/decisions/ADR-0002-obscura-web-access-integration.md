# ADR-0002 — Integrate obscura as weave's governed web-access capability (no V8 in core)

- **Status:** proposed — 2026-06-13 (owner-directed; implementation gated on the web-research items below)
- **Plane:** agent-mesh
- **Owner:** drdave
- **Scope:** weave (a thin, feature-flagged web-access governance seam) ↔ obscura (`obscura-mcp`, separate binary). No change to weave's core invariants.
- **Supersedes/relates:** weave ADR-0001 (handoff adoption); obscura charter ("lane-governed agent web-access capability"); the corrected north star (capsule + TASK-0001).

## Context

weave is the Rust-native superset of repowire for *local agent-to-agent* orchestration — but the mesh has **no first-class web reach**. That is weave's **domain weakness**: repowire offered an optional hosted relay for remote/web access; weave deliberately dropped the daemon/relay (no-daemon, DB-is-broker, local). A meshed agent can message, ask, lease, schedule, and run jobs — but it cannot *act on the web*.

Owner directive (2026-06-13): close that weakness by integrating **obscura** — a Rust-native, stealth, MCP-native headless browser (V8 + Chrome DevTools Protocol; ~30 MB RAM, anti-detect; a drop-in headless-Chrome replacement) whose `obscura-mcp` crate already exposes `browser_*` automation over MCP. The integration must NOT break weave's non-negotiables: one dependency-light static binary, no Python, no-shell argv-only spawning, MCP-native, heavyweight deps behind feature flags only.

## Decision

1. **weave does NOT link obscura / V8 into its binary.** V8 (~70 MB) in the default build would violate the dependency-light / one-small-static-binary invariant. `obscura-mcp` stays a **separate dependency-light Rust binary** — its own MCP server exposing `browser_navigate/click/fill/type/evaluate/snapshot/network_requests/console_messages/wait_for/press_key/select_option/close`.
2. **weave is the GOVERNANCE PLANE for stealth agent web access.** obscura-mcp is registered as a first-class **capability/peer** in the weave mesh; web-access requests flow through weave's *existing* primitives — `ask_permission`/`permission_*` (gate), `lease_*` (mutual exclusion / rate), `job_*` (durable dispatch + audit). Stealth web access — powerful and abuse-prone — is therefore gated, leased, and audited exactly like any other mesh work. This realizes obscura's own charter: *"lane-governed agent web-access capability."*
3. **Optional thin launcher, feature-flagged.** A `--features obscura` (or `weave web`) seam may spawn/attach `obscura-mcp` via **argv-only `std::process::Command` (never a shell)** and proxy `browser_*` behind the permission gate, so one weave install can bring *governed* web access online. The **default build excludes it** → dependency-light preserved.

## Alternatives considered (rejected)

- **Link obscura crates / V8 into weave core** — rejected: blows ~70 MB V8 into the default binary; violates dependency-light and the single-small-binary invariant.
- **Shell out to headless Chrome / Puppeteer / Playwright** — rejected: pulls a Node/Python runtime, heavyweight, not Rust-native, not stealth, and breaks the no-shell invariant.
- **Re-introduce a repowire-style hosted relay for web** — rejected: re-adds a daemon/service weave exists to avoid; obscura-as-capability gives reach without a relay.

## Consequences

- weave gains **governed** web reach with **zero added weight** to its core binary; both weave and obscura stay dependency-light, Rust-native, MCP-native, Python-free.
- weave becomes the **policy + audit plane** for agent web access (security-positive: stealth browsing must be gated, not ambient).
- Coordination is via MCP + the mesh (no code coupling); obscura remains independently usable; weave remains useful with no browser present (graceful degradation, as today).
- New work items (see backlog): capability registration + permission policy for web access; the optional `--features obscura` launcher (argv-spawn); a `browser_*`-through-permission proxy; tests (capability gating, no-shell spawn, default-build-excludes-V8).

## Research / Cross-References

- **Codebase (verified 2026-06-13):** obscura `meta/obscura` — 7-crate Rust workspace (`obscura-{dom,net,browser,cdp,js,mcp,cli}`), Apache-2.0 (compatible with weave's Apache/MIT dual), `obscura-mcp/src/lib.rs` MCP surface = the `browser_*` tools above; README perf table (30 MB / 70 MB binary / built-in anti-detect / Puppeteer+Playwright). obscura fleet capsule northstar: *"lane owns network engineering/control; obscura upgrades it with stealth agent web access"*, next_command *"charter as lane-governed agent web-access capability."*
- **weave invariants (CLAUDE.md / ARCHITECTURE.md §7):** one dependency-light Rust binary; no Python; no-shell, argv-only `Command::new(bin)`; MCP stdout discipline; heavyweight/tokio-tree deps behind feature flags (as `libsql` is); the permission/lease/job systems that become the governance primitives here.
- **repowire reference** (`.handoff/loop/_done/_workspace_prev/references/.../repowire.md`): its optional hosted relay for remote access is the capability gap weave's local model otherwise leaves — closed here by obscura-as-capability rather than a relay.
- **TO COMPLETE before proposed→accepted (web research per the org ADR rule):** (a) MCP server-to-server / capability-composition security patterns (a meshed obscura is an egress surface — confirm the permission gate is sufficient and how to scope it); (b) CDP anti-detection maintenance burden and legal/ToS posture of stealth scraping; (c) whether to depend on the `obscura-mcp` crate API directly vs. spawn-and-speak-MCP. These are flagged, not yet done — this ADR is *proposed*, not accepted.
