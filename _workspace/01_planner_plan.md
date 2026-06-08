# WL-017 Implementation Plan: Mesh Memory System

## Goal
Filesystem-backed scoped memory under `~/.config/weave/memory/` with CLI read/write/search and automatic context prefixing on ask delivery (repowire parity). Single-cycle scope: plain markdown files, simple substring search, no SQLite/FTS, no async.

---

## 1. Directory Structure

```
$XDG_CONFIG_HOME/weave/memory/          # fallback ~/.config/weave/memory/
├── global/
│   ├── onboarding.md
│   └── conventions.md
├── project/
│   └── weave/
│       └── agent-patterns.md
├── persona/
│   └── drdave/
│       └── preferences.md
└── orchestrator/
    └── default/
        └── runbook.md
```

- **Base dir**: `weave-core::config::config_dir()` (new helper returning the parent of `config_path()`).
- **Scopes**:
  - `global/` — no resolution needed.
  - `project/<repo_name>/` — resolved from cwd via `git::capture_worktree_tags` or `git::repo_name_from_toplevel`.
  - `persona/<identity>/` — resolved from `resolve_me()` / `cfg.session`.
  - `orchestrator/<circle>/` — resolved from `cfg.circle()`.

---

## 2. File Format

Each memory entry is a markdown file with YAML frontmatter:

```markdown
---
title: "Agent coding patterns"
tags: ["rust", "invariants", "mcp"]
created_ts: 1717785600
updated_ts: 1717872000
---

# Agent coding patterns

Always parameterize SQL…
```

- **Key → filename**: key `agent-patterns` → `agent-patterns.md`.
- **Frontmatter fields**: `title` (string), `tags` (array of strings), `created_ts` (i64 epoch), `updated_ts` (i64 epoch).
- **Body**: everything after the closing `---` of the frontmatter.
- **Encoding**: UTF-8 only.

---

## 3. Core Module (`weave-core/src/memory.rs`)

### 3.1 Types

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryScope {
    Global,
    Project(String),
    Persona(String),
    Orchestrator(String),
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub scope: MemoryScope,
    pub key: String,
    pub title: String,
    pub tags: Vec<String>,
    pub created_ts: i64,
    pub updated_ts: i64,
    pub body: String,
}
```

### 3.2 Public API

```rust
/// Return the absolute directory for a scope.
pub fn memory_dir(scope: &MemoryScope) -> PathBuf;

/// Return the absolute path for a scope + key.
pub fn memory_path(scope: &MemoryScope, key: &str) -> PathBuf;

/// Write or overwrite a memory entry. Creates parent dirs.
pub fn memory_write(
    scope: &MemoryScope,
    key: &str,
    title: &str,
    tags: &[String],
    body: &str,
) -> Result<()>;

/// Read a memory entry by scope + key.
pub fn memory_read(scope: &MemoryScope, key: &str) -> Result<MemoryEntry>;

/// Search memory entries in a scope (or all scopes if `scope = None`).
/// Simple substring grep over the full file contents (frontmatter + body).
/// Returns matches sorted by relevance (exact tag match > substring in body/title).
pub fn memory_search(scope: Option<&MemoryScope>, query: &str) -> Result<Vec<MemoryEntry>>;

/// List all entries in a scope.
pub fn memory_list(scope: &MemoryScope) -> Result<Vec<MemoryEntry>>;

/// Delete an entry. Returns true if it existed and was removed.
pub fn memory_delete(scope: &MemoryScope, key: &str) -> Result<bool>;

/// Return all available scopes that currently have at least one entry on disk.
pub fn memory_scopes() -> Result<Vec<MemoryScope>>;
```

### 3.3 Scope Resolution Helpers (for CLI/MCP consumption)

```rust
/// Resolve the current project scope from cwd (best-effort; returns None if not in a git repo).
pub fn project_scope_from_cwd() -> Option<MemoryScope>;

/// Resolve the current persona scope from an identity string.
pub fn persona_scope(identity: &str) -> MemoryScope;

/// Resolve the current orchestrator scope from a circle string.
pub fn orchestrator_scope(circle: &str) -> MemoryScope;
```

### 3.4 Internal Helpers

- `sanitize_key(key: &str) -> Result<String>`: validates key name. Reject empty, path traversal (`..`, `/`, `\`), and chars outside `[a-zA-Z0-9_-]`. Max 128 chars.
- `parse_entry(path: &Path) -> Result<MemoryEntry>`: reads file, splits YAML frontmatter, deserializes fields.
- `format_entry(entry: &MemoryEntry) -> String`: serializes frontmatter + body.
- `relevance_score(query: &str, entry: &MemoryEntry) -> u32`: exact tag match = 100, title substring = 50, body substring = 10.

---

## 4. Integration Points

### 4.1 Export from `weave-core`

Add `pub mod memory;` to `weave-core/src/lib.rs`.

### 4.2 CLI Commands (`weave/src/main.rs`)

Add to the `Cmd` enum:

```rust
/// Filesystem-backed scoped memory (global, project, persona, orchestrator).
Memory {
    #[command(subcommand)]
    cmd: MemoryCmd,
},
```

Add `MemoryCmd` enum:

```rust
#[derive(Subcommand)]
enum MemoryCmd {
    /// Write a memory entry.
    Write {
        #[arg(long)]
        scope: String, // "global" | "project" | "persona" | "orchestrator"
        #[arg(long)]
        key: String,
        #[arg(long, allow_hyphen_values = true)]
        title: String,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    /// Read a memory entry.
    Read {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
    },
    /// Search memory entries.
    Search {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// List memory entries in a scope.
    List {
        #[arg(long)]
        scope: String,
    },
    /// Delete a memory entry.
    Delete {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
    },
    /// Show available scopes and their resolved paths.
    Scopes,
}
```

**Dispatch** (`fn main` match arm):
- Parse `scope` string into `MemoryScope` using resolution helpers.
- `write`: call `memory_write`, print `"wrote <scope>/<key>"`.
- `read`: call `memory_read`, print frontmatter + body (or JSON with `--json` flag — add `json: bool` to each variant).
- `search`: call `memory_search`, print list of `scope/key/title/tags` (respect `--limit`).
- `list`: call `memory_list`, print table.
- `delete`: call `memory_delete`, print `"deleted <scope>/<key>"` or `"not found"`.
- `scopes`: print resolved paths for all 4 scope kinds (showing what project/persona/orchestrator would resolve to right now).

### 4.3 MCP Tools (`weave-mcp/src/mcp.rs`)

Add to `tools()` JSON schema:

- `weave_memory_write`
- `weave_memory_read`
- `weave_memory_search`
- `weave_memory_list`
- `weave_memory_delete`

Add to `call_tool()` dispatcher:

```rust
"weave_memory_write" => tool_memory_write(args),
"weave_memory_read" => tool_memory_read(args),
"weave_memory_search" => tool_memory_search(args),
"weave_memory_list" => tool_memory_list(args),
"weave_memory_delete" => tool_memory_delete(args),
```

Each tool function signature: `fn tool_memory_xxx(args: &Value) -> Result<String, String>`.

Tool semantics mirror CLI but accept `scope` as a string (same resolution rules). `weave_memory_search` takes optional `scope` and `query`, returns JSON array of results. `weave_memory_read` returns the full entry as JSON.

### 4.4 Context Prefixing on Ask Delivery

**Design**: Hook into the delivery path **before** `store.ask()` / `store.send()` is called. The prefixed body is what gets persisted and delivered, so the recipient sees the memory context inline.

**Implementation**:

1. Add a helper in `weave-core/src/memory.rs`:

```rust
/// Build a memory context prefix for a given body + current scopes.
/// Searches all resolved scopes (global, project from cwd, persona from identity,
/// orchestrator from circle) for entries whose tags/body match keywords extracted
/// from the body (or a dedicated `memory_query` field).
pub fn build_context_prefix(identity: &str, circle: &str, body: &str, top_n: usize) -> String;
```

2. **Keyword extraction** (simple): split body on whitespace, filter out stop words (the, a, is), take first 10 words as query tokens. Perform `memory_search(None, &keyword)` for each keyword, collect unique entries, score by relevance, sort, take top N.

3. **Delimiter format**:

```markdown
<weave-memory>
- [global::onboarding] Agent coding patterns
  Always parameterize SQL…
- [project::weave::agent-patterns] Test discipline
  Every change needs fmt+clippy+test…
</weave-memory>

<original body follows>

<the actual ask body here>
```

4. **Hook points**:
   - **CLI `Cmd::Ask`**: after resolving `from`, before `store.ask(...)`, call `build_context_prefix(&from, &cfg.circle(), &body, 3)` and prepend to `body`.
   - **CLI `Cmd::Send`**: same hook before `store.send(...)`.
   - **CLI `Cmd::Reply`**: same hook before `store.reply(...)`.
   - **MCP `tool_ask`**: before `store.ask(...)`, prepend context.
   - **MCP `tool_send`**: before `store.send(...)`, prepend context.
   - **MCP `tool_answer`**: before `store.send(...)` (answer uses the default `reply` impl which calls `send`), prepend context.

**Opt-out**: Add a `--no-memory` CLI flag and an `no_memory: Option<bool>` MCP arg to skip prefixing for a single call. Check in each hook; if true, pass body through unchanged.

---

## 5. Input Caps & Security

| Surface | Limit | Enforcement |
|---------|-------|-------------|
| Key name | ≤ 128 chars, `[a-zA-Z0-9_-]` only | `sanitize_key` rejects invalid chars and path traversal |
| Title | ≤ 256 chars | `sanitize_tag(title, 256)` |
| Tags | ≤ 16 tags, each ≤ 64 chars | truncate count and length |
| Tag chars | `[a-zA-Z0-9_-]` | strip invalid chars |
| Body | ≤ 64 KiB | reject if over cap |
| File per scope | ≤ 10,000 entries | `memory_list` caps traversal; reject write if over |
| Search results | ≤ 50 entries | hard cap in `memory_search` |
| Context prefix top-N | ≤ 5 entries | hard cap in `build_context_prefix` |
| Memory dir | Must be under `~/.config/weave/memory/` | `memory_path` resolves via `config_dir()` only; never accept user-supplied absolute paths as scope dirs |

**Path traversal defense**:
- `sanitize_key` rejects any key containing `/`, `\`, or `..`.
- `memory_path` builds the path by joining the resolved scope dir with `format!("{key}.md")`.
- Never interpret user input as a directory component beyond the validated key name.

---

## 6. Scope Resolution Details

| Scope | Resolution Rule |
|-------|-----------------|
| `global` | Always available. Dir = `~/.config/weave/memory/global/` |
| `project` | `git::capture_worktree_tags(&std::env::current_dir()?).repo`. If empty (non-git cwd), return an error telling the user to specify `global` or use `--cwd` in a git repo. |
| `persona` | `resolve_me(None, None, cfg)` (explicit > config > cwd basename). |
| `orchestrator` | `cfg.circle()` (validated, defaults to `"default"`). |

For CLI/MCP `scope` argument parsing:
- Exact values: `"global"`, `"project"`, `"persona"`, `"orchestrator"`.
- Any other value → error: `"scope must be one of: global, project, persona, orchestrator"`.

---

## 7. File Changes

### New files
- `weave-core/src/memory.rs` — core memory API

### Modified files
- `weave-core/src/lib.rs` — add `pub mod memory;`
- `weave-core/src/config.rs` — add `pub fn config_dir() -> PathBuf` helper
- `weave/src/main.rs` — add `MemoryCmd`, dispatch arm, context-prefix hooks in `Cmd::Send`, `Cmd::Ask`, `Cmd::Reply`
- `weave-mcp/src/mcp.rs` — add 5 memory tools to `tools()` and `call_tool()`, context-prefix hooks in `tool_ask`, `tool_send`, `tool_answer`

### No changes needed
- `weave-core/src/store.rs` — filesystem-based; no Store trait changes
- `weave-core/src/model.rs` — no new DB types

---

## 8. Test Plan

### Unit tests (`weave-core/src/memory.rs` inline `#[cfg(test)]`)

1. **`sanitize_key_accepts_good_rejects_bad`**: accepts `foo-bar_123`, rejects `../etc`, `foo/bar`, `foo\bar`, empty, over-length.
2. **`memory_roundtrip`**: write → read → assert equality.
3. **`memory_search_substring`**: write two entries, search for a unique substring, assert correct entry returned.
4. **`memory_search_tag_priority`**: write entries where one has a matching tag and the other only has body match; assert tag match ranks higher.
5. **`memory_list_and_delete`**: list entries, delete one, list again, assert gone.
6. **`memory_path_is_under_config_dir`**: assert all scope variants produce paths under `~/.config/weave/memory/`.
7. **`parse_entry_handles_no_frontmatter`**: graceful error for a file missing frontmatter.
8. **`parse_entry_handles_empty_body`**: frontmatter only, empty body is ok.

### Integration tests (`weave/tests/` or inline in `weave/src/main.rs` test module)

1. **`cli_memory_write_read`**: spawn `weave memory write --scope global --key test --title T --body B`, then `weave memory read --scope global --key test`, assert output contains title and body.
2. **`cli_memory_search`**: write multiple entries, `weave memory search --scope global --query foo`, assert matching keys printed.
3. **`cli_memory_scopes`**: run `weave memory scopes`, assert all 4 scope paths printed.

### Security tests

1. **`path_traversal_rejected`**: attempt write with key `../../../etc/passwd`, assert error.
2. **`oversized_body_rejected`**: attempt write with 100 KiB body, assert error.
3. **`bad_tag_chars_stripped`**: write tag `foo;rm -rf`, read back, assert `foorm-rf` or rejection.

### Context prefixing tests

1. **`ask_prefixes_memory`**: write a global memory entry, then call `tool_ask` (or CLI `weave ask`) with a body containing the keyword, inspect the persisted message body via store directly and assert the `<weave-memory>` block is present.
2. **`no_memory_opt_out`**: pass `no_memory: true` in MCP `weave_ask`, assert the persisted body does NOT contain `<weave-memory>`.

---

## 9. Single-Cycle Checklist

- [ ] `weave-core/src/memory.rs` created with full API
- [ ] `config_dir()` helper added
- [ ] `weave memory` CLI wired with all 6 subcommands
- [ ] 5 MCP memory tools wired in `mcp.rs`
- [ ] Context prefixing hooked in CLI send/ask/reply and MCP tool_send/tool_ask/tool_answer
- [ ] Input caps enforced (key, title, tags, body, file count)
- [ ] Unit tests for memory ops pass
- [ ] Integration tests for CLI pass
- [ ] Security tests for path traversal + caps pass
- [ ] `cargo fmt && cargo clippy -D warnings` clean
- [ ] `cargo test` passes on both sqlite and libsql backends (memory module is backend-agnostic, but verify no regressions)
