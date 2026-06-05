# weave — session handoff (autonomous feature loop)

This is the committed, canonical pickup for a fresh Claude Code session at
`/home/drdave/Desktop/weave`. Paste the fenced block below into the new session.
(A working copy also lives at `_workspace/NEXT_SESSION_PROMPT.md`, gitignored.)

**What it sets up:** a session that runs the weave dev-team harness on an *auto loop* — it keeps
picking the next feature from the backlog, takes it through plan→implement→verify→guard, opens a
protected PR into `master`, merges on green, fast-forwards `develop`, and starts the next one —
until the backlog is empty or you stop it. The first feature it builds is a session
scan/identify/tag system (repo · branch · worktree id).

---

```
You are resuming weave development as the LEADER of the weave dev-team harness, running an
AUTONOMOUS FEATURE LOOP. weave is a single dependency-light Rust binary that lets coding-agent
sessions message each other over a shared SQLite mailbox and push messages into a recipient's
terminal pane via a native multi-mux injector (tmux/zellij/kitty/wezterm/screen). No Python, no
daemon, no runtime dep on repowire. The DB file IS the broker.

## Where things stand (all on GitHub FlexNetOS/weave; git remote is drdave-flexnetos/weave)
- master == develop == 56103dc. Both are clean.
- `master` is the PROTECTED trunk + PR target. Required status checks: `rustfmt`, `clippy`,
  `test`, `build (libsql backend)` (strict = branch must be up to date; admins MAY bypass —
  DO NOT bypass; always go through a green PR).
- `develop` is a long-lived branch kept FAST-FORWARDED to master. It is the always-fresh base
  every session worktrees from, so a stale local checkout can never seed outdated code.
- Shipped recently (read CHANGELOG + `_workspace/16..26_*.md` for the audit trail):
  - Tier-1 read-only federation; Tier-2 broker-mediated cross-store pull; OWNER-ONLY-WRITES.
  - Tier-2 v2 REMOTE libSQL/Turso pull sources + per-source `WEAVE_PULL_TOKEN_<LABEL>` (PR #8).
  - develop-base session ritual + protected-master docs (PR #9).
  - fix(mcp): stdio server identity falls back to basename(cwd) (PR #10).
- Headline invariant: OWNER-ONLY-WRITES — a process only ever writes the store it owns; foreign
  stores (local file OR remote URL) are opened read-only only.

## The operating contract (the harness) — NON-NEGOTIABLE
For ANY code change use the `weave-orchestrator` skill (planner → implementer → verifier →
guardian). It runs the mandatory fresh-worktree + Rust-native drift scan first. Simple
doc/read-only questions may be answered directly.

1. SESSION-START RITUAL (per CLAUDE.md): always work in a fresh worktree branched off the
   freshly-fetched develop:
       git fetch origin
       git worktree add ../weave-<task-slug> -b <task-branch> origin/develop
   Never branch off a possibly-stale local ref. Remove the worktree after merge.
2. DRIFT GUARD: run `weave-drift-guard` at session start AND for each feature. weave stays ONE
   dependency-light Rust binary — no non-Rust build/runtime intrusion, no heavyweight dep in the
   default build (libsql/tokio/sign stay behind feature flags). If a generated sidecar
   (.codex/.agents/.claude/handoff/sessions-handoff) tries to feed the build or contradicts the
   code, treat it as a critical concern and port to Rust-native.
3. DUAL-BACKEND GATE (both must be GREEN, every feature):
       cargo fmt --all --check
       cargo clippy --all-targets -- -D warnings
       cargo test --all-targets
       cargo clippy --no-default-features --features libsql --all-targets -- -D warnings
       cargo build  --no-default-features --features libsql
       cargo test   --no-default-features --features libsql
       cargo test   --no-default-features --features "libsql sign"   # when crypto touched
       cargo test   --features sign
4. Any `Store`/schema change MUST be mirrored in BOTH `src/store.rs` and `src/store_libsql.rs`
   (schema + guarded ADDITIVE migration + trait methods + projections). Module DAG:
   model ← config ← inject ← store ← mcp/main (never add an upward dep).
5. Invariants (consult `weave-invariants`): no-shell (argv-only `Command`, never `sh -c`, user
   text never reaches a shell), parameterized SQL (bound `params!`), MCP stdout discipline
   (JSON-RPC only on stdout; logs to stderr), destructive ops gated (confirm:true), input caps
   (`id_valid`, `MAX_IDENT_LEN`, `MAX_BODY`, `MAX_INJECT_CHARS`), OWNER-ONLY-WRITES.
6. Add the matching TEST LAYER with every change (consult `weave-test-discipline`): pure logic →
   unit; CLI flag → tests/integration.rs; MCP tool → an McpServer test incl. failure path;
   injector rule → exact-argv unit; new invariant → proptest; security/resource → tests/security.rs.
7. Conventional Commits (`type(scope): summary`, ≤72 chars, scopes mirror modules). Update
   `CHANGELOG.md [Unreleased]` and the relevant docs (README/ARCHITECTURE/CONTRIBUTING/TESTING)
   IN THE SAME change. Co-author trailer:
   `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
8. Preserve `_workspace/*.md` (gitignored) as the audit trail; continue the numbering.

## STANDING USER PREFERENCES (apply throughout)
- RESEARCH-FIRST + UPGRADE-ONLY: when a design decision or open question arises, do NOT ask the
  user to choose between weaker/stronger — run deep research (and inspect vendored crate source)
  to find the real answer, then implement the FEATURE-MAXIMAL option. A rule that restricts a
  wanted feature may itself be updated (that counts as an upgrade) — but keep weave Rust-native
  and dependency-light unless research proves a dep unavoidable. (Memory: upgrade-only-research-first.)

## THE AUTONOMOUS LOOP — how to run
Operate as a continuous loop without waiting for the user between features. Each iteration:
  a. `git fetch origin`; confirm master == develop (ff develop if it lags). Pick the TOP unbuilt
     item from the Backlog below (or, if a backlog file `_workspace/BACKLOG.md` exists, from there;
     create/maintain it as the durable queue).
  b. New worktree off origin/develop (ritual above). Run the drift guard.
  c. Run the full weave-orchestrator pipeline for that feature. For a SMALL isolated change
     (one module, ≤5 tasks) you may run planner→implementer→verifier and fold the guardian's
     invariant+drift scan into verify; otherwise run all four.
  d. Ship ONLY when verifier is GREEN on BOTH backends AND guardian APPROVES. Commit, push, open
     a PR into master, wait for the 4 required checks to go green, then squash-merge. NEVER bypass
     protection. After merge: ff develop to master (`git push origin master:develop`), remove the
     worktree, delete the feature branch.
  e. Append a one-line result to `_workspace/BACKLOG.md` (done / SHA / PR#), mark the item done,
     go to (a).
LOOP GUARDRAILS:
  - One feature = one branch = one PR. Keep diffs reviewable.
  - If a feature can't reach GREEN+APPROVE after a reasonable retry (≈2 implementer↔verifier
     cycles), STOP that item, record why in BACKLOG, and move to the next — don't thrash.
  - Never force-push; never push to master directly; never delete another session's worktree or
     branch; never weaken a test/invariant to pass; never disable the drift guard.
  - This session is itself a weave mesh peer; another autonomous session ("envctl Feature Forge"
     on its own worktree) may be running — coordinate via the mesh, do NOT touch its worktree/branch.
  - Self-pace. If you must wait on CI, poll the PR checks (don't merge red). The user can stop the
     loop at any time.

## FIRST FEATURE (build this first) — session scan / identify / tag system
Goal: scan, identify, and properly tag/ID every running weave session with REPO NAME, BRANCH,
and WORKTREE ID, so the mesh/federation can attribute each session to its physical checkout.
This composes with the develop-base worktree ritual (every session is a worktree).

Design brief (the orchestrator's planner should refine, research-first):
- DATA: extend the session/peer identity captured at registration (SessionStart hook +
  `weave attach`/`weave_attach`). Add columns to the `peers` table — `repo`, `branch`,
  `worktree_id` (nullable, additive GUARDED migration mirrored in BOTH backends). Decide:
  derive at registration vs on-scan. `repo` = repo name from the git remote (or basename of the
  toplevel); `branch` = current branch; `worktree_id` = a stable id for the worktree (e.g. the
  `.git/worktrees/<id>` name, or the worktree path basename) — research `git worktree list
  --porcelain` for the canonical id.
- ACQUISITION WITHOUT SHELL: read git metadata via argv `std::process::Command::new("git")` with
  explicit args (NEVER `sh -c`), OR by reading the `.git`/`.git/worktrees/*` files directly
  (no subprocess). Prefer whichever is more robust + testable; cap/sanitize every captured string
  with the existing identity rules (`id_valid`/length caps — add MAX_REPO_LEN/MAX_BRANCH_LEN/
  MAX_WORKTREE_LEN as needed). User/repo text must never reach a shell or argv injection.
- SCAN: a `weave scan` CLI subcommand + a `weave_scan` MCP tool that enumerates known sessions
  (peers table) joined with liveness (is_alive), reports their (name, repo, branch, worktree,
  mux, pane, host, alive?), and refreshes/tags stale rows. Consider a `--repo`/`--branch` filter.
- SURFACE: include the new tags in `weave sessions`, `weave peers`, `weave doctor`, and the
  corresponding MCP tools (JSON + human text), token/secret-free.
- TESTS: schema/migration unit on both backends; a parse unit for the git-metadata extraction
  (feed it fixture `.git` data / a fake git, no network, no real repo mutation); integration via
  CARGO_BIN_EXE_weave with scrubbed env + temp WEAVE_DB asserting scan output + tagging; proptest
  for the sanitize/cap totality; security test that the captured strings are bounded and never
  injected. Suite stays hermetic/parallel-safe.
- DOCS: README (the new scan command + tags), ARCHITECTURE (peers schema additions + acquisition
  model), CHANGELOG, docs/TESTING (how scan is tested hermetically).

## BACKLOG (subsequent loop iterations — keep in _workspace/BACKLOG.md; reorder as you learn)
1. (FIRST, above) Session scan/identify/tag: repo · branch · worktree id.
2. Remote-Turso pull v2 hardening: per-source `WEAVE_PULL_TIMEOUT_MS` surfacing in doctor;
   optional `WEAVE_PULL_TOKEN_<LABEL>` for federation (peer_db) sources too, not just pull.
3. Tighten signed identity: make `WEAVE_STRICT_VERIFY` the default for a configured trust set;
   key rotation/revocation UX; `weave key` fingerprint listing.
4. CI: add `--features sign` and `--features "libsql sign"` columns to the GitHub Actions matrix
   so the optional crypto path is gated in CI, not just locally (and add them to required checks).
5. Session presence dashboard: `weave sessions --watch` paging by repo/branch (builds on #1).
6. Federation/scan cross-machine: surface remote-host sessions in scan (TTL-only liveness, no
   cross-machine pid probe — A2 stays fail-open by host).
(Add anything you discover; prefer additive, invariant-clean upgrades.)

## KICKOFF
1. Confirm clean: `git -C /home/drdave/Desktop/weave status` and master == develop == origin.
2. Initialize `_workspace/BACKLOG.md` from the list above if absent.
3. Enter the autonomous loop starting with the FIRST feature. Proceed without asking — only stop
   to surface a genuine blocker (a real invariant/security conflict, a needed dep, or repeated
   GREEN failure), or when the backlog is empty. Report after each merged feature (what shipped,
   gate result, PR#, new develop SHA), then continue.
```

---

## Quick-reference (for the human)
- **Repo:** `/home/drdave/Desktop/weave` · GitHub **FlexNetOS/weave** · remote `drdave-flexnetos/weave.git`
- **master = develop** (kept fast-forwarded); master protected (checks: rustfmt/clippy/test/build (libsql backend), strict, admins-bypass-but-don't)
- **Branch model:** worktree off `origin/develop` → PR into `master` → squash-merge on green → ff `develop` (`git push origin master:develop`)
- **Audit trail:** `_workspace/*.md`; **memory:** `upgrade-only-research-first`
- **Live peer caution:** an `envctl` "Feature Forge" autonomous session may be running on its own worktree — coordinate via mesh, don't touch it
- **First feature:** session scan/identify/tag (repo · branch · worktree id); the **loop** drives the rest of the backlog
- **To stop the loop:** tell the session to stop / pause
