# Plan: WL-029 — Advisory file leases with TTL expiry and conflict detection

## Objective
Extend the WL-024 lease system with file-path-aware conflict detection (parent/child prefix matching), automatic expiry sweep, and explicit sweep tooling.

## Architecture

### Model
- `lease_path_normalize(r: &str) -> String` — strip trailing slashes, collapse multiple slashes, reject `..` and empty segments.
- `lease_path_conflicts(existing: &str, candidate: &str) -> bool` — true if exact match, or one is a parent/ancestor of the other (prefix + '/').

### Store trait
- `sweep_expired_leases(&self) -> Result<usize>` — delete all rows where `expires <= now()`, return count deleted.
- `reserve_lease` already checks exact-match conflicts; extend it to check prefix conflicts via `lease_path_conflicts`.
  - On conflict, error message includes holder name and expiry time of the conflicting lease.
- `list_leases` should auto-sweep before listing (defensive hygiene).

### Schema
No schema change — `leases` table already has `expires` (WL-024).

### sqlite backend
- `sweep_expired_leases`: `DELETE FROM leases WHERE expires <= ?1`
- `reserve_lease`: after exact-match check, run prefix-conflict query:
  ```sql
  SELECT holder, expires FROM leases WHERE expires > ?1
    AND (resource = ?2 OR resource || '/' = SUBSTR(?2, 1, LENGTH(resource) + 1)
         OR ?2 || '/' = SUBSTR(resource, 1, LENGTH(?2) + 1))
  ```
  If any row returned, bail with conflict info.
- Auto-sweep at top of `list_leases` and `reserve_lease`.

### libsql backend
- Mirror all sqlite changes (async-over-block_on bridge).

### CLI
- New `LeaseCmd::Sweep` variant — `weave lease sweep` prints count of expired leases removed.
- `weave lease reserve` failure already prints error; conflict info will now include holder + expiry.

### MCP
- New `weave_lease_sweep` tool in schema + dispatch.

### Test layers
- Unit: `lease_path_normalize` and `lease_path_conflicts` edge cases.
- Integration:
  - `cli_lease_path_conflict_parent_child` — reserve `/foo/bar`, fail to reserve `/foo/bar/baz`.
  - `cli_lease_path_conflict_child_parent` — reserve `/foo/bar/baz`, fail to reserve `/foo/bar`.
  - `cli_lease_sweep_removes_expired` — reserve with 1s TTL, wait, sweep removes it.
  - `mcp_lease_sweep_roundtrip` — MCP tool call.
- Full gate on both backends.

## Invariants
- No shell: all external programs via `Command::new(bin)` with explicit argv.
- Dual-backend: every Store trait change mirrored in sqlite + libsql.
- Input caps: paths capped at `MAX_LEASE_RESOURCE_LEN` (512), already enforced.
- Parameterized SQL only.
