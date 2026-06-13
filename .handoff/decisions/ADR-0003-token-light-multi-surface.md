# ADR-0003 — Token-light by architecture: full-feature multi-surface with progressive-disclosure MCP

- **Status:** accepted — 2026-06-13 (owner-directed: "full adoption, no half measures, all features"; implementation tracked in WL-050..052)
- **Plane:** agent-mesh
- **Owner:** drdave
- **Scope:** weave's agent-facing surfaces (CLI, MCP, HTTP/dashboard, bots) and a new first-class invariant. No change to the core data model / no-daemon / no-shell invariants.
- **Supersedes/relates:** ADR-0002 (obscura governance — the token argument compounds here); the corrected north star (capsule + TASK-0001). Adds **token-light** as a peer of **dependency-light**.

## Context

Owner challenge (2026-06-13): *"MCP is really a token suck. Not so bad if it's local, but is that really an upgrade?"* — and the directive to resolve it with **full adoption, no half measures, all features pulled in**.

The critique is correct and measurable. MCP's cost is the **standing tool table** — every tool's name + description + JSON input-schema is injected into the model context for the whole session, paid again on every prompt-cache miss, *independent of whether anything is called*. Local vs. remote transport fixes **latency**, not **tokens** — the token tax is the schema-in-context.

Quantified (web research, below): each MCP tool ≈ **550–1,400 tokens**; dozens of tools commonly burn **30–50%** of the context window before the first message (one reported case: **72%**, 143k/200k); GitHub Copilot hard-caps at **128 tools** to prevent this. weave already exposes **70 `weave_*` tools** (≈ 38k–98k standing tokens, worst case), and the mission *adds* surface — obscura `browser_*`, agent spawn/kill, human-surface controls. Naïvely, "more features" → "more standing tax" → a real downgrade against the RTK token-frugality ethos this whole environment is built on.

The resolution is NOT to amputate the surface (the half-measure the owner explicitly rejected). It is to **keep every feature and remove the standing tax by architecture** — the industry-proven progressive-disclosure pattern achieves **85–98% token reduction with zero capability loss** ("47 tools → 2 tools = 141k → 1.6k tokens").

## Decision

1. **Full-feature multi-surface parity — no surface is a reduced subset.** Every weave capability is reachable via **(a) CLI** (primary, ~zero standing token cost), **(b) MCP** (full, progressive-disclosure), **(c) HTTP/SSE dashboard** (Rust-native, over `weave-mcp/http.rs`), **(d) Telegram/Slack bridges** (Rust-native). Surfaces are **feature-flagged for build leanness** (default build stays dependency-light), but no surface is feature-reduced. "More than repowire, not less" holds on every surface.

2. **`token-light` is a first-class invariant, peer of `dependency-light`.** The **standing context cost** of weave's MCP surface must stay small — **budget target ≤ ~2k tokens** — regardless of how many capabilities exist. *Adding a feature must not add standing tokens.* A ballooning tool table is the same species of drift as a heavyweight dependency, and is guarded the same way (review + a budget check).

3. **MCP via progressive disclosure (meta-tool / dispatcher), not 70 eager flat tools.** Replace the flat table with a tiny standing surface that exposes the FULL operation set with schemas fetched on demand:
   - a small set of **namespaced dispatcher tools** (`weave_msg`, `weave_ask`, `weave_peer`, `weave_job`, `weave_lease`, `weave_orchestrate`, `weave_review`, `weave_schedule`, `weave_memory`, `weave_permission`, `weave_daemon`, `weave_summarize`, `weave_admin`, `weave_web`), each taking an `action` discriminator — **all 70+ operations preserved**, table shrinks ~5×; and/or
   - a **meta-tool** (`weave` with modes `search` / `describe` / `call`) so per-operation schemas load only when the agent asks — the 85–98% reduction path.
   - Keep a **backward-compatible eager-flat mode behind a flag** for harnesses that require flat tools (no capability or compatibility lost).

4. **CLI is the zero-standing-cost path (RTK-aligned).** Full CLI parity is mandatory; `rtk weave …` compresses output. Token-sensitive agents call `weave <subcmd>` via the shell and pay tokens only for the command + its output, only when used. This — not MCP — is the default recommendation for token-bound sessions.

5. **Code-execution option (Anthropic "code execution with MCP").** Agents may drive weave from code (the CLI binary / a thin client) instead of per-op tool calls — a further token lever, naturally afforded by the full CLI.

6. **obscura stays governed (ADR-0002) — the token win compounds.** The agent never loads obscura's `browser_*` table; web access flows through weave's (now progressive-disclosure) `weave_web` dispatcher + permission/lease/job gate. Governed capability = web reach with near-zero added standing tokens.

## Consequences

- weave gains every feature on every surface **and** stays token-light — the "is MCP really an upgrade?" tension is resolved: MCP becomes an upgrade *because* its standing cost is engineered down to a couple thousand tokens while the CLI offers a zero-cost path.
- Standing MCP cost drops from ~tens-of-thousands of tokens to a small bounded budget; adding obscura / spawn-kill / surfaces no longer taxes context.
- Cost: dispatcher/meta-tool calls are slightly less self-describing than flat tools (the model must learn `action` values or `search` first) — mitigated by `describe`/help and the CLI. Backward-compat flat mode covers harnesses that need it.
- New invariant to enforce in review (weave-guardian / CLAUDE.md): a **standing-token budget** for the MCP surface, checked when tools are added.

## Alternatives considered (rejected)

- **Slim the surface / expose only a core subset / drop tools** — the half-measure the owner explicitly rejected: it loses features to save tokens. Progressive disclosure gets the tokens back *without* the loss.
- **MCP-as-is (70 eager flat tools)** — rejected: the measured token suck; gets worse as the mission adds surface; bumps toward the 128-tool ceiling.
- **Drop MCP, CLI-only** — rejected: some agents only speak MCP, and structured/validated calls have real value; the answer is full MCP done token-light, plus full CLI.

## Research / Cross-References

- **Web (2026-06-13):** modelcontextprotocol issue #2808 — "tool schema token overhead (~1000 tokens/tool/session)"; Anthropic Engineering, "Code execution with MCP: building more efficient AI agents" (up to 98% reduction); MindStudio, "Optimize MCP Server Token Usage" (code execution / tool search / TOON); apideck, "Your MCP Server Is Eating Your Context Window… CLI alternative"; demiliani, "MCP and the 'too many tools' problem"; Solo.io & SynapticLabs, progressive-disclosure / meta-tool pattern ("47 tools → 2 tools, 98% reduction"); jenova.ai, "AI Tool Overload: more tools = worse performance"; GitHub Copilot's 128-tool hard cap.
- **Codebase (verified 2026-06-13):** `weave-mcp/src/mcp.rs` (70 `weave_*` tools, ~179 KB source incl. schemas); `weave/src/main.rs` (~40 CLI subcommands — full parity already exists); `weave-mcp/src/http.rs` (the HTTP seam for the dashboard); the permission/lease/job systems (the governance primitives reused by `weave_web`); RTK (`rtk weave`, the token-killer ethos this ADR aligns to).
- **Sources:** https://github.com/modelcontextprotocol/modelcontextprotocol/issues/2808 · https://www.anthropic.com/engineering/code-execution-with-mcp · https://www.apideck.com/blog/mcp-server-eating-context-window-cli-alternative · https://demiliani.com/2025/09/04/model-context-protocol-and-the-too-many-tools-problem/ · https://www.solo.io/blog/mcp-progressive-disclosure · https://blog.synapticlabs.ai/bounded-context-packs-meta-tool-pattern · https://www.mindstudio.ai/blog/optimize-mcp-server-token-usage
