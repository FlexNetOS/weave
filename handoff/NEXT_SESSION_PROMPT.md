# Next-session pickup prompt — weave presence/federation work

> Paste the block below into a fresh Claude Code session at `/home/drdave/Desktop/weave`.
> Everything above the line is orientation for you (the human); the fenced block is the prompt.

---

```
You are resuming weave development. weave is a single dependency-light Rust binary that lets
coding-agent sessions message each other over a shared SQLite mailbox and push messages into a
recipient's terminal pane via a native multi-mux injector. Operate via the `weave-orchestrator`
skill for any code change (it runs the mandatory fresh-worktree + Rust-native drift scan first,
then the planner → implementer → verifier → guardian team, with the dual-backend gate:
fmt + clippy -D warnings + test on BOTH sqlite and libsql).

## Where things stand (all merged to master)
Two feature packages shipped in the previous sessions:
- PR #5 (commit 16362b4) — Presence & Live-Connect: A1 heartbeat-on-read, A2 real liveness
  (peers.pid/host, is_alive = TTL ∧ local-pid-alive via Linux /proc, fail-open remote),
  B1 `weave attach`/`weave_attach` (zero-restart adoption), C2 `weave connect`/`weave_connect`
  (capability verdict + graceful queue), FR6 doctor non-default-WEAVE_DB hint, and Tier-1
  read-only federation (`WEAVE_PEER_DBS`, origin-tagged dedup union, open_readonly both backends).
- PR #6 (commit a73e355) — Tier-2 cross-store delivery (Option C broker-mediated request-pull):
  outbox + authorized send, pull/commit/dedup (open_readonly source → commit to LOCAL inbox →
  advance pull_cursor; idempotent, at-least-once bounded to one intent per crash), injection
  DEFAULT ON (allowlist-gated, paste-safe Nudge only, `WEAVE_INJECT_PULLED`/`allow_inject_from`),
  and signed identity behind an OPTIONAL `sign` Cargo feature (ed25519-dalek; default build stays
  crypto-free; `keys` table; `weave key gen|show|add|list`; `WEAVE_STRICT_VERIFY`).
The headline invariant introduced by Tier-2 is OWNER-ONLY-WRITES: a process only ever writes the
store it owns; foreign stores are only ever opened read-only (proven byte-unchanged on both
backends). master is clean; full design/audit trail (PRDs, plans, verifier/guardian reports) is in
`_workspace/*.md` (gitignored).

## Loose ends to address first (in priority order)
1. **`gen` is an edition-2024 reserved keyword.** `tests/integration.rs` (~lines 2367, 2439) and
   `tests/security.rs` (~line 1308) bind `let gen = run_ok_env(...)` and call `pubkey_from_gen(&gen)`.
   Compiles fine on edition 2021 (current) — all tests green — but rust-analyzer reports false
   "expected pattern" syntax errors, and it would break on a move to edition 2024. Trivial fix:
   rename the local `gen` binding (e.g. `keygen`) in those 3 spots + their call sites. Low risk;
   good first task to confirm the harness/gate is working.
2. **Two merged remote branches still exist** — `feat/presence-live-connect` and
   `feat/tier2-cross-store-delivery`. Delete them if desired (requires explicit user OK;
   `git push origin --delete <branch>`).
3. **master has no branch protection** — PRs #5 and #6 merged before CI finished because no
   required status checks are configured. If CI should gate merges, add branch protection on
   master requiring the `test` / `clippy` / `rustfmt` / `build (libsql backend)` checks. Verify the
   post-merge runs are green: `gh run list`.

## Candidate next features (pick with the user)
- **Remote-Turso cross-store pull (Tier-2 v2).** Current v1 limit: pull/federation sources are
  LOCAL-FILE stores only (open_readonly). Extending pull to a remote libSQL/Turso URL is the
  natural next increment for true cross-machine delivery. Note the design caveat: A2 pid-liveness
  is meaningless cross-machine (already fail-open by host), and `open_readonly` semantics for a
  remote libsql source need design — start with a planner pass referencing
  `_workspace/06_planner_tier2_trust_model.md` and `_workspace/08_planner_tier2_build.md`.
- **Tighten signed identity:** make `WEAVE_STRICT_VERIFY` the default for a configured trust set,
  key-rotation/revocation UX, and `weave key` listing of fingerprints.
- **CI: add `--features sign` (and `libsql sign`) gate columns** to the GitHub Actions matrix so
  the optional crypto path is covered in CI, not just locally.

## How to work
- ALWAYS start via `weave-orchestrator` for code changes; it enforces the fresh worktree
  (`git worktree add ../weave-<slug> -b <branch>`) and the Rust-native drift scan. Simple
  doc/read-only questions can be answered directly.
- The dual-backend gate is non-negotiable: sqlite AND `--no-default-features --features libsql`,
  plus `--features sign` when touching crypto.
- Any Store/schema change must be mirrored in BOTH `src/store.rs` and `src/store_libsql.rs`
  (schema + guarded additive migration + trait methods + projections).
- Commit/PR/merge only when the user asks. Conventional Commits; update CHANGELOG [Unreleased] and
  the relevant docs in the same change.

Start by reading `_workspace/NEXT_SESSION_PROMPT.md` and the latest `_workspace/*.md` artifacts for
full context, confirm master is clean (`git -C /home/drdave/Desktop/weave status`), then ask me
which loose end or feature to take — or, if I said nothing specific, propose fixing loose-end #1
(the `gen` rename) as a quick warm-up and wait for my go.
```

---

## Quick-reference facts (for the human)
- **Repo:** `/home/drdave/Desktop/weave` · **master:** `a73e355` (Tier-2) on top of `16362b4` (Tier-1)
- **Merged PRs:** #5 (presence/connect/Tier-1 federation), #6 (Tier-2 cross-store delivery)
- **Remote:** `https://github.com/drdave-flexnetos/weave.git` (PRs render under FlexNetOS/weave)
- **Audit trail:** `_workspace/01..15_*.md`, `PRD_presence_live_connect.md`,
  `06_planner_tier2_trust_model.md`, `08_planner_tier2_build.md`
- **Top loose end:** rename the `gen` test binding (edition-2024 keyword) — trivial, not a current bug
