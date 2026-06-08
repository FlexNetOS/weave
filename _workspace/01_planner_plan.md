# WL-018 Implementation Plan: Birth Certificates / Runtime Identity Envelopes

## Goal
Prevent path-based identity takeover by minting unguessable nonces at peer registration. A peer's first registration generates a birth certificate; subsequent registrations for the same identity must present the matching cert. Backward-compatible: existing peers without a cert are upgraded on first re-registration.

## Attack Vector
Bob knows Alice's identity name. Bob runs `weave attach --name alice` or registers via MCP. The store's UPSERT blindly overwrites Alice's peer row. Messages to Alice now route to Bob's pane. There is currently no proof-of-ownership.

---

## 1. Schema Changes

### peers table: add `birth_cert` column

Following the existing additive migration pattern (socket, pid, host, repo, branch, worktree_id, circle, role, turn_state, description, description_ts all use `TEXT NOT NULL DEFAULT ''`):

```sql
ALTER TABLE peers ADD COLUMN birth_cert TEXT NOT NULL DEFAULT '';
```

- Added to `SCHEMA` constant in both backends (`store.rs` for sqlite, `store_libsql.rs` for libsql) immediately after `description_ts`.
- Added to `migrate()` in both backends with `column_exists` / `pragma_table_info` guard (sqlite) or `SELECT 1 FROM pragma_table_info('peers')` guard (libsql).
- Empty string `''` means "not yet enrolled" (backward-compat), matching the precedent of `host`, `repo`, `socket`, etc.

### New constant

```rust
pub const MAX_BIRTH_CERT_LEN: usize = 64;
```

Add to `weave-core/src/model.rs` alongside `MAX_REPO_LEN`, `MAX_BRANCH_LEN`, etc.

---

## 2. Nonce Generation

Use `getrandom` (already in `weave-core/Cargo.toml` as optional for `sign`). Make it **unconditional**:

```toml
# weave-core/Cargo.toml
[dependencies]
getrandom = "0.2"
```

It's tiny (~1 dep, no-std) and cryptographically secure. Remove `optional = true`.

**Hex encoding (zero new deps)**: 32 random bytes -> 64 hex chars = exactly at cap. Inline a small hex encoder in `weave-core/src/store.rs` since `to_hex` lives in the `sign` feature-gated module.

```rust
fn mint_birth_cert() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf)
        .map_err(|e| anyhow::anyhow!("birth cert entropy failure: {e}"))?;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in buf {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    Ok(out)
}
```

Also add a validation helper:
```rust
fn check_birth_cert(cert: &str) -> Result<()> {
    if cert.len() > MAX_BIRTH_CERT_LEN {
        anyhow::bail!("birth certificate too long ({} chars; max {})", cert.len(), MAX_BIRTH_CERT_LEN);
    }
    if !cert.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("birth certificate must be hex digits only");
    }
    Ok(())
}
```

---

## 3. Store Trait Changes

### `register_peer_full` signature change

```rust
#[allow(clippy::too_many_arguments)]
fn register_peer_full(
    &self,
    name: &str,
    mux: &str,
    target: &str,
    socket: &str,
    cwd: Option<&str>,
    pid: Option<i64>,
    host: &str,
    repo: &str,
    branch: &str,
    worktree_id: &str,
    circle: &str,
    birth_cert: Option<&str>, // NEW: proof cert for re-registration
) -> Result<String>; // NEW: returns the peer's effective birth cert
```

**Semantics**:
1. **Validate inputs** first: `check_ident("peer name", name)?`, sanitize tags, validate circle, `check_birth_cert(cert)?` if `Some`.
2. **Read existing row** in a transaction:
   - `SELECT birth_cert FROM peers WHERE name = ?`
3. **If no existing row** (INSERT path):
   - Mint new cert via `mint_birth_cert()`.
   - `INSERT INTO peers (... , birth_cert) VALUES (... , ?)`
   - Return the new cert.
4. **If existing row with `birth_cert == ''`** (legacy upgrade path):
   - Mint new cert via `mint_birth_cert()`.
   - `UPDATE peers SET mux=?, target=?, ..., birth_cert=? WHERE name=?`
   - Return the new cert.
5. **If existing row with `birth_cert != ''`** (enforced path):
   - If `birth_cert` arg is `None` -> **reject**: `anyhow::bail!("peer '{}' already has a birth certificate; provide it to re-register", name)`
   - If arg provided but doesn't match stored -> **reject**: `anyhow::bail!("birth certificate mismatch for peer '{}'", name)`
   - If matches -> `UPDATE peers SET mux=?, target=?, ... (birth_cert omitted from SET) WHERE name=?`
   - Return the existing cert (unchanged).

**Important**: `role` continues to be omitted from the UPDATE SET (preserves orchestrator). `birth_cert` is also omitted from the UPDATE SET in the enforced path (the cert never changes after minting).

### `register_peer` wrapper update

```rust
fn register_peer(&self, name: &str, mux: &str, target: &str, socket: &str, cwd: Option<&str>) -> Result<String> {
    self.register_peer_full(name, mux, target, socket, cwd, None, "", "", "", "", "default", None)
}
```

This preserves backward-compat for existing test call sites that create fresh peers. Tests that **re-register** the same peer will need to capture the cert from the first call and pass it on the second call.

---

## 4. Backend Implementation Details

### SqliteStore (`weave-core/src/store.rs`)

- Update `SCHEMA` DDL for `peers` table: add `birth_cert TEXT NOT NULL DEFAULT ''` after `description_ts`.
- Update `migrate()`: add idempotent `ALTER TABLE peers ADD COLUMN birth_cert TEXT NOT NULL DEFAULT ''` guarded by `column_exists`.
- Update `register_peer_full` impl to use a `Transaction` (not a blind UPSERT):
  - `tx.query_row` to probe existing `birth_cert`.
  - Branch on the three cases above.
  - `tx.execute` for INSERT or UPDATE.
  - `tx.commit()`.
- **Do NOT add `birth_cert` to `Peer`**. Do NOT change `row_to_peer`. Inside `register_peer_full`, run a standalone `SELECT birth_cert FROM peers WHERE name=?` before the write transaction. This avoids touching `Peer`, `row_to_peer`, `get_peer`, and `list_peers` at all.

### LibsqlStore (`weave-core/src/store_libsql.rs`)

Mirror every sqlite change exactly:
- Update `SCHEMA` constant.
- Update migration block (the `pragma_table_info` probe + `ALTER TABLE` pattern).
- Update `register_peer_full` impl with the same transaction logic.
- Do NOT touch `row_to_peer` or `Peer`.

---

## 5. Call Site Updates

The following call sites invoke `register_peer_full`. All must be updated to pass `birth_cert: Option<&str>` and handle the returned `String`.

### CLI `Cmd::Register` (`weave/src/main.rs` ~3119)

Add `--cert` flag:
```rust
Cmd::Register {
    #[arg(long)] name: Option<String>,
    #[arg(long)] cwd: Option<String>,
    #[arg(long)] cert: Option<String>, // NEW
}
```

- Pass `cert.as_deref()` to `register_peer_full`.
- On success, print: `registered '{me}' [...] (birth-cert: {cert})`

### CLI `Cmd::Attach` (`weave/src/main.rs` ~3140)

Add `--cert` flag (same struct pattern as Register).
- Pass `cert.as_deref()` to `register_peer_full`.
- On success, print: `attached '{me}' [...] (birth-cert: {cert})`

### Hook `session` (`weave/src/main.rs` ~4023)

- Read `WEAVE_BIRTH_CERT` env var: `std::env::var("WEAVE_BIRTH_CERT").ok()`
- Pass as `birth_cert` to `register_peer_full`.
- On success, if cert was newly minted (i.e. env was empty), print: `[weave] registered peer '{me}' [...] (save birth-cert: {cert})`
- If cert was from env and matched, print the normal message (no need to print cert again).

### CLI `Cmd::Scan` self-refresh (`weave/src/main.rs` ~2985)

- Read `WEAVE_BIRTH_CERT` env var.
- Pass to `register_peer_full`.
- Error is swallowed as today (non-fatal). After cert is set, missing env -> error -> swallowed -> self-refresh silently skipped. This is acceptable.

### CLI `Cmd::Sessions --watch` self-refresh (`weave/src/main.rs` ~2794)

Same as Scan: read `WEAVE_BIRTH_CERT`, pass, swallow error.

### MCP `tool_attach` (`weave-mcp/src/mcp.rs` ~1685)

- Accept `cert` in tool JSON schema: add `"cert":{"type":"string","description":"Your birth certificate (omit on first attach)."}` to `weave_attach` inputSchema.
- Read from args: `args.get("cert").and_then(|v| v.as_str())`
- Pass to `register_peer_full`.
- On success, return text: `Attached '{me}' ... (birth-cert: {cert})`

### MCP `tool_scan` self-refresh (`weave-mcp/src/mcp.rs` ~1145)

- Read `WEAVE_BIRTH_CERT` env var.
- Pass to `register_peer_full`.
- Error swallowed as today.

**Note**: MCP `initialize` (JSON-RPC method) does **NOT** call `register_peer_full` -- it only returns protocol capabilities. No change needed there.

---

## 6. Input Caps and Validation

| Surface | Limit | Enforcement |
|---------|-------|-------------|
| `birth_cert` value | <= 64 chars, hex `[0-9a-fA-F]` only | `check_birth_cert()` at store seam; rejects oversized / non-hex before any DB write |
| `birth_cert` arg | Optional | `Option<&str>`; `None` triggers legacy-upgrade or rejection logic |
| `name` | existing `MAX_IDENT` (64 chars, no control chars) | `check_ident` (unchanged) |

---

## 7. Test Plan

### Unit tests (`store.rs` / `store_libsql.rs`)

- **`register_peer_mints_cert`**: first `register_peer("a", ...)` returns a 64-char lowercase hex string.
- **`register_peer_rejects_re_register_without_cert`**: register "a", then call `register_peer_full(..., None)` again -> error mentioning "birth certificate".
- **`register_peer_accepts_matching_cert`**: register "a", capture cert, re-register with `Some(&cert)` -> success, same cert returned.
- **`register_peer_rejects_wrong_cert`**: register "a", re-register with `Some("0000...")` -> error "mismatch".
- **`register_peer_upgrades_legacy`**: manually INSERT a peer row with `birth_cert=''`, then call `register_peer(...)` -> success, cert minted and returned.
- **`register_peer_full_preserves_cert_on_update`**: register, capture cert, re-register with matching cert -> `get_peer` shows updated mux/target, cert unchanged.

**Test migration**: Update existing tests that call `register_peer` / `register_peer_full` twice for the **same** peer name to capture the cert from the first call and pass it on the second. Affected tests (from grep audit):
- `register_peer_full_roundtrips_pid_and_host`
- `register_peer_full_roundtrips_git_tags`
- `register_peer_full_preserves_circle_on_upsert`
- `turn_state_and_description_preserved_on_re_register`
- `register_peer_rejects_bad_socket`
- `legacy_db_without_pid_host_migrates_in_place` (re-register after migration)
- Plus libsql mirrors of the above.

For tests that register **different** peer names each time, no change needed (first call is always an INSERT).

### Integration tests

- **`cli_attach_mints_cert`**: run `weave attach --name test1` -> stdout contains `birth-cert:` and a 64-char hex token.
- **`cli_attach_rejects_takeover`**: attach `alice`, capture cert. In a subprocess, `weave attach --name alice` (no cert) -> error exit code + stderr about missing cert.
- **`cli_attach_accepts_cert`**: attach `alice` with `--cert <captured>` -> success.
- **`hook_session_mints_cert`**: run hook session with empty `WEAVE_BIRTH_CERT` -> stderr contains new cert.
- **`hook_session_rejects_without_cert`**: run hook session again with empty env -> stderr contains error, peer row NOT updated.

### Security tests

- **`cert_oversized_rejected`**: pass 65-char cert -> store rejects before DB touch.
- **`cert_non_hex_rejected`**: pass `gggg...` -> store rejects.
- **`cert_never_in_peers_list`**: register peer, call `list_peers` -> assert no `birth_cert` field in JSON (if serialized) or in output.
- **`mcp_attach_takeover_blocked`**: MCP `tool_attach` for existing peer without cert -> error after first attach set the cert.

---

## 8. Files to Touch

| File | Change |
|------|--------|
| `weave-core/Cargo.toml` | Remove `optional = true` from `getrandom` |
| `weave-core/src/model.rs` | Add `MAX_BIRTH_CERT_LEN = 64` |
| `weave-core/src/store.rs` | `mint_birth_cert()`, `check_birth_cert()`, trait sig, `register_peer` wrapper, schema DDL, `migrate()`, `register_peer_full` impl (transaction logic) |
| `weave-core/src/store_libsql.rs` | Mirror all store.rs changes |
| `weave/src/main.rs` | Add `--cert` to `Cmd::Register` and `Cmd::Attach`; pass cert from `WEAVE_BIRTH_CERT` env to hook session, scan, sessions --watch; print returned cert |
| `weave-mcp/src/mcp.rs` | Add `cert` to `weave_attach` schema; read `WEAVE_BIRTH_CERT` in `tool_scan`; pass cert arg in `tool_attach`; return cert in tool text |
| `weave-core/src/store.rs` (tests) | Update re-register tests to capture + pass cert; add new cert-specific tests |
| `weave-core/src/store_libsql.rs` (tests) | Mirror all new / updated tests |

---

## 9. Single-Cycle Scope Confirmation

- **No new heavy dependencies**: `getrandom` is already a dependency; we only remove `optional = true`.
- **No breaking existing users on upgrade**: legacy peers (empty `birth_cert`) get a cert minted on their next re-registration automatically.
- **No Peer struct / JSON wire changes**: `birth_cert` is **not** added to `Peer`; it never leaks in listings, serializations, or public APIs.
- **Both backends**: sqlite + libsql migrations and implementations.
- **All registration paths covered**: CLI register, CLI attach, hook session, scan self-refresh, sessions watch self-refresh, MCP tool_attach, MCP tool_scan self-refresh.
- **Security invariants honored**: cert is secret, capped, validated, never exposed in read paths.

**Estimated LOC**: ~500-600 lines (trait + 2 backends + 2 frontends + migrations + tests).
