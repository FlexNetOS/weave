---
repo: "numman-ali/cc-mirror"
url: "https://github.com/numman-ali/cc-mirror"
language: "typescript | shell"
last_scanned: "2026-06-07"
scan_agent: "agent-4s6zbyrw"
status: "active"
---

# cc-mirror — Feature Inventory

**Elevator pitch:** Create multiple isolated Claude Code variants with custom providers
(Z.ai, MiniMax, OpenRouter, LiteLLM) — each with its own config, theme, model mapping,
and privacy hardening.

---

## 1. Feature Inventory

### Isolation & Variants
- **Multiple isolated Claude Code variants** on one machine
- **Binary isolation** — each variant installs its own native Claude Code runtime (version-pinnable)
- **Environment isolation** — per-variant env vars (base URLs, model mappings, privacy flags)
- **Runtime version pinning** — track `stable`, `latest`, or pin specific version (e.g., `2.1.37`)
- **Update policy** — refresh managed defaults while preserving credentials and user-added MCPs

### Provider Support
- Z.ai, MiniMax, OpenRouter, LiteLLM, Kimi, and more
- **Provider-native prompt packs** — minimal system-prompt overlays per provider
- **Model slot mapping** — maps Anthropic model env vars to provider equivalents
- **Provider tool blocking** — writes `permissions.deny` to push models toward provider-native tools
- **Z.ai CLI integration** — `Z_AI_API_KEY` + `zai-cli` commands (search, read, vision, repo)

### Customization
- **Brand themes via tweakcc** — signature color palette, thinking verbs, spinner style per provider
- **tweakcc deep integration** — patches Claude Code binary for themes, system prompts, UX tweaks,
  tool descriptions, input highlighters, thinking verbs
- **ASCII splash art** — per-provider wrapper splash on TTY launch
- **Skill installation** — optional `dev-browser` skill into variant config

### Tooling
- **Interactive TUI wizard** (Ink/React-based) for discovery, creation, management
- `cc-mirror doctor` — sanity-checks all variants
- **Shell env integration** — optional injection of variant env into shell profiles
- **API-key onboarding bypass** — stores key suffix in `.claude.json` to skip OAuth
- **Privacy hardening** — disables auto-updater, telemetry, error reporting per variant

---

## 2. Weave Overlap

| cc-mirror Feature | Weave Equivalent | Notes |
|-------------------|------------------|-------|
| Multi-variant Claude Code | — | **Gap**: weave assumes single Claude Code install |
| Provider-specific config | — | **Gap**: no provider abstraction |
| Theme customization | — | **Gap**: no theming |
| Version pinning | — | **Gap**: no version management |
| Privacy hardening | — | **Gap**: no telemetry disable automation |

---

## 3. Weave Gaps

### Low Impact (out of scope)
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **Multi-variant support** | weave is a plugin, not a launcher; variants are external concern |
| 2 | **Theme / UX customization** | weave is backend infrastructure, not frontend |
| 3 | **Provider tool blocking** | Model-specific permissions are outside weave's scope |
| 4 | **Version pinning** | Claude Code's concern, not weave's |

---

## 4. Proposed WL Items

None directly. cc-mirror is a Claude Code launcher/orchestrator; weave is a session-mesh
messaging layer. They are orthogonal.

Potential minor integration:
- `weave setup` could detect cc-mirror presence and offer per-variant MCP registration

---

## 5. Integration Opportunities

- cc-mirror variants could each have their own weave peer identity
- `weave setup --variant <name>` could register MCP per cc-mirror variant
- Both tools harden privacy (telemetry disable) — shared conventions possible

---

## 6. Notes

- cc-mirror is a **launcher/orchestrator** for Claude Code instances
- weave is a **messaging mesh** between existing sessions
- No direct feature competition; they are stack-adjacent
- The most interesting pattern is **per-variant identity isolation** — weave's peer registry
  could support variant-scoped identities if needed
