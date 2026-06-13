# ADR-0002 — Integrate obscura as weave's governed web-access capability (no V8 in core)

- **Status:** accepted — 2026-06-13 (implemented as WL-049; web-research items resolved below)
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

### Resolved decisions (WL-049, 2026-06-13)

The three "TO COMPLETE" open questions are now resolved:

- **(c) crate-dep vs. spawn-and-speak → SPAWN-AND-SPEAK stdio MCP.** weave does NOT depend on the
  `obscura-mcp` crate API (that would link V8/CDP/tokio into weave). Instead, behind a default-OFF
  `--features obscura`, weave spawns the separate `obscura` binary (`obscura mcp [--stealth]
  [--proxy P] [--user-agent UA]`) as a child via **argv-only `std::process::Command`** and acts as a
  minimal hand-rolled **MCP client** speaking newline-delimited JSON-RPC over the child's stdio,
  built on `std::io` + the already-present `serde_json`. **Zero new runtime dependencies; no tokio, no
  async, no V8.** The `cargo tree` of the default build is byte-identical before/after; the obscura
  feature adds no external crate.
- **Governance:** deny-by-default. Every `browser_*` op flows through weave's existing
  permission / lease / job primitives (`Store::ask` + `permission_verdict`, `reserve_lease` /
  `release_lease`, `create_job` / `update_job`) — **no new Store method, no schema change, dual-backend
  unaffected**. A pure `weave-core::webpolicy` module (no I/O) makes the allow decision and runs an
  SSRF/loopback URL validator.
- **Agent surface:** ONE token-light dispatcher — a single `weave_web {action, args, describe?}` MCP
  tool (added to `DANGEROUS_TOOLS`) plus a `weave web <op>` CLI subcommand — proxying all 35
  `browser_*` ops, NOT 35 eager tools (ADR-0003 progressive disclosure via `describe:true`).

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
### Web research — resolved (WL-049, 2026-06-13)

**(a) MCP server-to-server / capability-composition security & SSRF in headless browsers — CLOSED.**
When weave composes a downstream capability server (obscura) and forwards tool calls to it, weave
becomes a *confused-deputy* / egress surface: the downstream browser can be steered to fetch
attacker-chosen URLs. The consulted guidance converges on three concrete mitigations, all of which
this integration implements:

1. *Default-deny + explicit capability scoping at the composing host.* The MCP threat literature
   (Anthropic Model Context Protocol security guidance; the MCP specification's "Security and Trust &
   Safety" section, modelcontextprotocol.io) stresses that a host bridging to another MCP server must
   **not** grant the downstream server ambient authority — each forwarded capability must be
   explicitly consented and auditable. weave realizes this with deny-by-default `webpolicy` +
   the permission/lease/job gate: no web op runs unless explicitly allowed, and every op is auditable.
2. *SSRF defense by destination allow/deny-listing.* OWASP's SSRF Prevention Cheat Sheet
   (cheatsheetseries.owasp.org) and the OWASP Web Security Testing Guide recommend blocking requests
   to loopback (`127.0.0.0/8`, `::1`), link-local (`169.254.0.0/16` incl. the `169.254.169.254` cloud
   metadata endpoint), and RFC1918 private ranges (`10/8`, `172.16/12`, `192.168/16`) **by default**,
   allowing only an explicit allow-list of public hosts. weave's `webpolicy::web_url_ok` does exactly
   this for every URL-bearing op (default-deny internal/localhost/link-local/private/`*.local`/bare-IP
   unless `obscura_policy.allow_internal=true`), so a meshed obscura cannot be used to reach internal
   services or the cloud-metadata endpoint.
3. *Process isolation + least privilege of the downstream server.* General defense-in-depth guidance
   (PortSwigger Web Security Academy SSRF material; GHSA advisories on headless-browser SSRF in
   automation tools) recommends running the browser as a separate, isolated process rather than
   in-process. weave's spawn-and-speak model keeps obscura (and V8/CDP) in a **separate child process**
   with argv-only launch from a trusted directory, no shell, child stdout consumed as a pipe (never
   re-emitted on weave's stdout), and child stderr/tokens redacted out of weave's logs.

**Finding:** the deny-by-default policy + SSRF/loopback validator + isolated argv-only child process
are the standard, sufficient mitigations for this composition. *Residual:* SSRF allow-listing is only
as good as the configured allow-list, and DNS-rebinding to an internal IP after the policy check is a
known residual for any URL-string validator (the check sees the hostname, the browser resolves it);
operators reaching truly sensitive internal services should additionally network-isolate the obscura
host. This is documented, not silently accepted.

*Sources consulted:* MCP specification & Anthropic MCP security guidance (modelcontextprotocol.io);
OWASP SSRF Prevention Cheat Sheet & Web Security Testing Guide (owasp.org); PortSwigger Web Security
Academy — SSRF (portswigger.net).

**(b) CDP stealth anti-detection / legal-ToS posture — DOCUMENTED RESIDUAL RISK (not a code gate).**
Stealth scraping carries operational and legal exposure (site Terms-of-Service violations, anti-bot
arms race, potential CFAA-class arguments in some jurisdictions). weave **cannot and does not** enforce
a site's ToS — that is the operator's responsibility. weave's contribution is *governance, not a legal
shield*: every web op is non-ambient (deny-by-default), gated (permission), optionally rate-limited
(lease), and audited (job event log), which is security-positive and improves accountability. The
maintenance burden of CDP anti-detection lives entirely in obscura (a separate binary on its own
release cadence); weave is insulated from it by the spawn-and-speak boundary. Operators must ensure
their use of stealth browsing is lawful and ToS-compliant for their targets.
