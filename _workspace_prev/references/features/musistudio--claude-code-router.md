---
repo: "musistudio/claude-code-router"
url: "https://github.com/musistudio/claude-code-router"
language: "typescript"
last_scanned: "2026-06-07"
scan_agent: "agent-o9i20fni"
status: "active"
---

# Claude Code Router (CCR) — Feature Inventory

**Elevator pitch:** Decouples Claude Code from Anthropic — intercepts native Anthropic-format
requests and translates them for any OpenAI-compatible provider, enabling custom model
routing, scenario-based selection, and subagent-specific models.

---

## 1. Feature Inventory

### Provider Routing
- **Multi-provider**: OpenRouter, DeepSeek, Ollama, Gemini, Volcengine, SiliconFlow,
  ModelScope, DashScope, AIHubMix, Groq, generic OpenAI-compatible
- **Scenario-based routing**:
  - `default` — general tasks
  - `background` — lightweight/cheap models
  - `think` — reasoning models for Plan Mode
  - `longContext` — large-context models when tokens > 60K
  - `webSearch` — models with web search
  - `image` — vision-capable models
- **Project-level overrides** — `~/.claude/projects/<id>/claude-code-router.json`
- **Custom router scripts** — JS module receives request+config, returns `provider,model`
- **Subagent routing** — `<CCR-SUBAGENT-MODEL>provider,model</CCR-SUBAGENT-MODEL>` in prompt
- **Dynamic switching** — `/model provider,model_name` mid-session
- **Proxy support** — `PROXY_URL` for upstream requests

### Request/Response Transformers
- Built-in: `anthropic`, `deepseek`, `gemini`, `openrouter`, `groq`, `maxtoken`, `tooluse`,
  `reasoning`, `enhancetool`, `cleancache`, `sampling`, `vertex-gemini`, etc.
- Transformer scoping: global, per-model, or custom options
- Custom transformer plugins via `config.json`
- Token estimation with `tiktoken` (cl100k_base)
- Streaming (SSE) handling: parse, rewrite, re-serialize

### Surfaces & Tooling
- `ccr code` — launch Claude Code pre-routed
- `ccr activate` — emit env vars for bare `claude` command
- `ccr ui` — React + Vite web management interface
- `ccr model` — interactive terminal UI for model management
- `ccr statusline` — runtime monitoring from stdin JSON
- **Non-interactive / CI mode** — `NON_INTERACTIVE_MODE`
- **GitHub Actions integration** — trigger Claude Code tasks in CI via `@claude` mentions
- Config backup (last 3 versions)

### Security
- Authentication gate: optional `APIKEY` with Bearer or x-api-key
- Without API key, host forced to `127.0.0.1`
- Dual logging: HTTP (pino) + application-level

---

## 2. Weave Overlap

| CCR Feature | Weave Equivalent | Notes |
|-------------|------------------|-------|
| Provider routing | — | **Gap**: weave is not a model router |
| Request transformers | — | **Gap**: no payload transformation |
| Subagent model override | — | **Gap**: no subagent-specific config |
| CI mode | — | **Gap**: no CI automation |
| Web UI | — | **Gap**: no web management interface |

---

## 3. Weave Gaps

### Low Impact (out of scope for weave)
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **Model routing / provider abstraction** | weave is a messaging mesh, not a model proxy |
| 2 | **Request/response transformers** | API format translation is orthogonal to messaging |
| 3 | **Web UI for config management** | weave is CLI-first by design |
| 4 | **CI/CD automation** | GitHub Actions integration is a separate concern |

---

## 4. Proposed WL Items

None directly. CCR solves a different problem (model routing vs. agent mesh). However:
- `WL-042` (multi-provider hook templates) could include CCR-compatible launch configs

---

## 5. Integration Opportunities

- CCR and weave are **complementary**: CCR routes models, weave routes messages between sessions
- A CCR-enabled Claude Code session can use `weave` MCP tools alongside custom providers
- weave could detect CCR presence and suggest provider-agnostic messaging

---

## 6. Notes

- CCR is a model-proxy infrastructure tool; weave is a session-mesh messaging tool
- No direct feature gaps to close; the tools serve different layers of the stack
- The only overlap is "make Claude Code more powerful" — they do it via different mechanisms
