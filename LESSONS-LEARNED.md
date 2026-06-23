# Lessons learned

## 2026-06-23 — CC Switch integration must start from the supplied repo, not a narrow bridge assumption

### What went wrong

The user supplied the full CC Switch repository archive at `cc-switch-main.zip` and
asked for the repo to be integrated/wired as-is. I treated CC Switch primarily as
a SQLite provider table plus a small `weave provider-switch` bridge. That was too
narrow. It missed the actual architecture in the restored archive:

- CC Switch is a full Tauri app (`package.json` v3.16.3), not just a DB.
- Its backend owns provider storage, app-specific live config writers, MCP sync,
  prompt sync, skill sync, proxy takeover, failover/circuit breakers, health,
  usage logs, and DeepLink import.
- Its supported app surface is wider than the current Weave bridge:
  `claude`, `claude-desktop`, `codex`, `gemini`, `opencode`, `openclaw`, and
  `hermes` appear in the archive, while Weave currently bridges only
  `claude`, `codex`, and `gemini`.
- By summarizing the bridge without mapping the full upstream repo, I created the
  false impression that the requested CC Switch wiring was smaller and more
  complete than it really was.

### Correct rule going forward

When the user supplies a whole upstream repo/archive and says to integrate it
"as-is":

1. Inspect the archive/repo first and map its real components before proposing or
   implementing a bridge.
2. Preserve the upstream ownership boundary. Do not flatten a product into one
   convenient table or API.
3. Build diagrams from source paths and schemas, not from memory or from the
   already-written partial bridge.
4. State explicitly which upstream capabilities are wired, read-only, ignored,
   unsupported, or deferred.
5. Never remove or discard supplied upstream evidence. If the archive should not
   be committed because it is large or third-party source, keep it local and add
   an ignore rule rather than deleting it.
6. For CC Switch specifically, any future Weave work must account for:
   provider catalog, current-provider markers, common config, live config writers,
   local proxy/takeover, provider health, failover, usage logs, MCP/prompt/skill
   sync, DeepLink import, and the wider app set.

### Current corrective action

`docs/ARCHITECTURE-GRAPHS.md` now contains a source-derived CC Switch graph using
`cc-switch-main.zip` as evidence, including the upstream Tauri app layout, SQLite
SSOT tables, live config writers, proxy/failover flow, and the exact gap between
CC Switch's real scope and Weave's current `provider-switch` bridge.

The archive is intentionally not vendored into Git in this docs-only correction:
it is about 25 MiB and is third-party source. It remains local evidence and is
ignored as `cc-switch-main.zip` so the Weave worktree can stay clean without
removing the user's file.
