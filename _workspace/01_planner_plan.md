# Plan: WL-028 — FTS5 full-text search on messages

## Objective
Enable fast full-text search across message body, subject, and sender fields using
SQLite FTS5. Provide CLI (`weave search`) and MCP (`weave_search`) interfaces.

## Architecture

### Schema (both backends)
- New `messages_fts` FTS5 virtual table shadowing `messages`:
  ```sql
  CREATE VIRTUAL TABLE messages_fts USING fts5(
    body, subject, sender,
    content='messages',
    content_rowid='id'
  );
  ```
- Triggers keep `messages_fts` in sync on INSERT/UPDATE/DELETE:
  ```sql
  CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body, subject, sender)
    VALUES (new.id, new.body, new.subject, new.sender);
  END;
  CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body, subject, sender)
    VALUES ('delete', old.id, old.body, old.subject, old.sender);
  END;
  ```
- Guarded migration: `CREATE VIRTUAL TABLE IF NOT EXISTS` + trigger `IF NOT EXISTS`.

### Store trait
- New method: `fn search(&self, query: &str, limit: i64) -> Result<Vec<Message>>`
- sqlite: `SELECT * FROM messages WHERE id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2)`
- libsql: same SQL (libsql FFI includes FTS5 constants; verified in target/debug/build/libsql-ffi-*/out/bindgen.rs)

### CLI
- `Cmd::Search { query: String, limit: i64, json: bool }`
- `weave search "hello world"` → human-readable list
- `weave search "hello world" --json` → JSON array
- `weave search "hello world" --limit 20` (default 50, capped at MAX_LIMIT)

### MCP
- `weave_search` tool in `tools()` schema
- `tool_search(store, args)` → JSON result string

### Test layers
- Unit: `store.search` roundtrip in both backends
- Integration: `cli_search_finds_sent_message` — send a message, search for its body
- Security: oversized query rejected, hostile query sanitized

## Invariants (from weave-invariants skill)
- No shell: query text never reaches a shell
- Parameterized SQL: `MATCH ?1`
- Input caps: query length capped, limit clamped
- No upward deps: `model` ← `store` ← `mcp`/`main`

## Dual-backend
- FTS5 virtual table + triggers in both `store.rs` (sqlite) and `store_libsql.rs` (libsql)
- libsql verified: libsql-ffi build output contains `FTS5_TOKENIZE_QUERY` etc.

## Files to touch
- `weave-core/src/store.rs` — schema migration, Store trait + impl, `search` method
- `weave-core/src/store_libsql.rs` — schema migration, async `search` impl
- `weave/src/main.rs` — `Cmd::Search` + dispatch arm
- `weave-mcp/src/mcp.rs` — `weave_search` tool schema + handler
- `weave/tests/integration.rs` — integration test
- `weave/tests/security.rs` — security test

## Migration strategy
- Additive only: `CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts`
- Triggers with `IF NOT EXISTS` guard
- Backfill existing messages: `INSERT INTO messages_fts(rowid, body, subject, sender) SELECT id, body, subject, sender FROM messages`
