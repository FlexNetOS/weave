# Guardian review — WL-013

## Invariants audit
- No shell involvement (only env var reads).
- No SQL changes.
- No store→inject edge changes (detect_target is pure env-var reading).
- Input caps: `Mux::parse` already validates; garbage falls through to `Mux::None`.
- MCP stdout discipline unchanged.

## Drift scan
- No new files.
- `detect_target()` signature changed but backward-compat wrapper preserved.

## Docs sync
- Config template updated.
- `ARCHITECTURE.md` injector section already documents all five backends.

## Verdict
**APPROVE** — clean config-only change with good test coverage.
