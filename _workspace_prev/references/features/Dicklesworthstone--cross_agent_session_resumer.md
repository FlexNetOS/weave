---
repo: "Dicklesworthstone/cross_agent_session_resumer"
url: "https://github.com/Dicklesworthstone/cross_agent_session_resumer"
language: "rust"
last_scanned: "2026-06-07"
scan_agent: "agent-h61aecio"
status: "active"
---

# Cross-Agent Session Resumer (casr) — Feature Inventory

**Elevator pitch:** Resume AI coding sessions across providers — converts Claude, Codex,
Gemini, Cursor, Aider, and 8 other session formats through a canonical IR so you can
pick up where you left off in any tool.

---

## 1. Feature Inventory

### Provider Support
- **14 providers** read/write: Claude Code, Codex, Gemini CLI, Cursor, Cline, Aider,
  Amp, OpenCode, ChatGPT, ClawdBot, Vibe, Factory, OpenClaw, Pi-Agent
- **Bidirectional conversion** — any source ↔ any target
- **Auto-detection** of source provider by session ID
- **Ergonomic shorthand flags**: `casr -cc <id>`, `casr -cod <id>`, etc.
- Same-provider conversion short-circuits gracefully

### Canonical IR Format
- `CanonicalSession`: `session_id`, `provider_slug`, `workspace`, `title`, `started_at`,
  `ended_at`, `messages`, `metadata`, `source_path`, `model_name`
- `CanonicalMessage`: `idx`, `role`, `content`, `timestamp`, `author`, `tool_calls`,
  `tool_results`, `extra`
- **Content normalization**: strings, text blocks, Codex `input_text`, tool-use blocks,
  ChatGPT `parts`
- **Timestamp normalization**: epoch seconds/ms, floats, numeric strings, RFC3339, ISO-8601
- **Role normalization**: maps provider roles to canonical enum; semantic buckets for
  lossy read-back verification

### Conversion Pipeline
- Resolve target → Resolve source → Read to IR → Validate → Optional enrich →
  Dry-run short-circuit → Same-provider short-circuit → Write target-native →
  Read-back verification → Output resume command
- **Synthetic enrichment** (`--enrich`): prepend synthetic context/orientation messages
- **Git repo discovery** (unreleased): auto `.git` detection enriches metadata

### Safety & Quality
- **Atomic writes**: temp → `fsync` → rename; conflict detection; `.bak` backup; auto rollback
- **Read-back verification**: re-reads output, compares structural fidelity before success
- **Semantic role buckets**: verification compares intent, not exact strings
- **Validation**: hard-stop on empty/one-sided sessions; warnings for gaps, odd ordering
- **Dry-run support** (`--dry-run`)
- **Round-trip invariant**: `read_P(write_P(canonical)) ~= canonical`

### Tooling
- `casr list` — workspace-scoped ranking with probe caps
- Machine-friendly `--json` mode (versioned, typed envelope)
- Structured tracing: `--verbose`, `--trace`, `RUST_LOG`
- Hardened `curl | bash` installer: SHA256, Sigstore/cosign, airgap, proxy-aware
- AGENTS.md with CI: integration/roundtrip/e2e, performance regression gates, Sigstore signing

---

## 2. Weave Overlap

| casr Feature | Weave Equivalent | Notes |
|--------------|------------------|-------|
| Session portability | — | **Gap**: weave sessions are store-bound, not portable |
| Canonical IR | — | **Gap**: no session interchange format |
| Provider abstraction | — | **Gap**: weave is Claude Code-centric |
| Read-back verification | — | **Gap**: no write verification pattern |

---

## 3. Weave Gaps

### Medium Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 1 | **Session export / import** | Move message history between weave instances or providers |
| 2 | **Canonical session IR** | Standard format for archiving, migration, tool interoperability |
| 3 | **Read-back verification** | Verify writes (e.g., config updates, hook rewrites) before declaring success |

### Low Impact
| # | Gap | Why It Matters |
|---|-----|----------------|
| 4 | **Multi-provider support** | weave is Claude Code hooks + MCP; other agents need custom wiring |

---

## 4. Proposed WL Items

- `WL-040` — Session export/import in canonical format (JSON/IR)
- `WL-041` — Read-back verification for destructive operations (setup, config rewrite)
- `WL-042` — Codex / Gemini CLI / Aider hook templates (multi-provider lifecycle hooks)

---

## 5. Integration Opportunities

- casr could convert weave's SQLite message history into other providers' formats
- weave could emit canonical session snapshots for casr to consume
- Both are Rust; shared serialization crates are feasible

---

## 6. Notes

- Session resuming is a different problem domain than live messaging; overlap is at the
  data portability layer, not the runtime layer
- The atomic-write + read-back verification pattern is worth adopting for `weave setup`
