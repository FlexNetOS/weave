# HANDOFF — weave (repowire-superset mission session, COMPLETE)

closed_utc: 2026-06-13T21:10Z
branch: develop @ 1a9bc1f (== master — converged)
worktree: main checkout /home/drdave/Desktop/meta/weave (all session worktrees removed)
cycle_budget: n/a (interactive, owner-driven one card at a time)
cycles_total: 4 cards (WL-046..049) + 4 runtime /verify fixes
cycles_this_session: WL-046, WL-047, WL-048, WL-049, + kill-fix + SSRF-fix + SSRF-QA + domain-wildcard
last_item: WL-049 (obscura governed web access) — merged, hardened, and /verify-audited
next_item: WL-050 (token-light progressive-disclosure MCP, ADR-0003) — owner's call
orchestrator_phase: complete (plan→implement→verify→guard ran for WL-047/048/049)
gate_status: PASS — master==develop==1a9bc1f, all six required checks green on the tip
pr_url: (none open) — PRs #72–#80 all merged

## Landed this session (all merged to develop; master fast-forwarded)
- #72 docs: restate canonical docs to the repowire-superset north star (WL-046)
- #73 docs: provable repowire-superset parity matrix `docs/REPOWIRE-PARITY.md` (WL-046)
- #74 weave: agent spawn/kill — `weave_spawn_peer`/`weave_kill_peer` (WL-047)
- #75 weave: Rust-native human surfaces — dashboard + Telegram/Slack (WL-048)
- #76 fix(inject): `weave kill` no longer falsely reports success on mux failure (found by /verify)
- #77 feat(weave): WL-049/ADR-0002 governed obscura web-access seam  ⚠️ agent-self-delivered pre-review
- #78 fix(webpolicy): close SSRF encoded-loopback bypass + WL-049 QA layers (closes #77's hole)
- #79 chore(handoff): session checkpoint at WL-049
- #80 fix(webpolicy): support `*` wildcard in obscura_allow_domains (found by /verify — domain `*` footgun)

## State (verified)
- master == develop == **7413553**, working tree clean, 0 open PRs, only the main worktree.
- Repowire-superset scorecard: **35/36 have-or-superset + governed web access** (beyond repowire's hosted relay). Remaining repowire gaps: 2 minor conveniences (`agents create` scaffold, `SOUL.md` persona file).
- New surfaces all behind feature flags, default build dependency-light + token-light: `surfaces` (dashboard/bots), `obscura` (governed web). Default `cargo tree` unchanged.

## Decisions (also in ICM: decisions-weave / errors-resolved / context-weave)
- WL-049 obscura: **spawn-and-speak stdio MCP, not a crate dep** — weave spawns `obscura mcp` (separate binary) argv-only and is a minimal hand-rolled MCP **client** (std::io + serde_json, NO tokio/V8 in weave). ADR-0002 accepted.
- All web surfaces are CLI subcommands + **one** dispatcher tool (`weave_web`), never 35 eager MCP tools (ADR-0003 token-light).
- Deny-by-default governance reuses the existing permission/lease/job Store methods + SSRF webpolicy; no new Store method/schema.

## Dead-ends / hazards (do not re-litigate / re-trip)
- **Agent self-delivery hazard** (ICM + memory `agent-self-delivery-hazard.md`): a weave-orchestrator SUBAGENT pushed+PR'd+auto-merged WL-049 (#77) before the guardian reviewed → a vulnerable SSRF version reached develop. FIX FORWARD: the leader owns delivery; tell subagents "no git push / gh pr"; verify diff-math at delivery to detect sneak-pushes. Consider denying `Bash(git push:*)`/`Bash(gh pr:*)` for weave-* subagents.
- **CI duplicate-run flake**: `ci.yml` triggers on BOTH `push:["**"]` and `pull_request` with NO `concurrency:` group → two racing runs cancel each other's jobs (hit #74,#76,#78). Recommended fix: `push: [master, develop]` (sync-master needs trunk-push checks) + `concurrency: {group: ci-${{github.ref}}, cancel-in-progress: true}`. Workaround used: re-run the failed job, or amend to a fresh SHA to clear a latched stale failure.
- **tmux/zellij socket not captured** (backlog WL-053): peer targets carry the pane id, not the mux socket, so inject/spawn/kill rely on ambient $TMUX → wrong server from a non-default socket / different session. The #76 fix makes kill *fail honestly*; the underlying limitation is WL-053 (dual-backend schema add).

## icm_stored
- context-weave (01KV19N7…, 01KV1DQG…), errors-resolved (01KV19NA…, 01KV1DQD…), decisions-weave (01KV19ND…)
- /verify found 4 runtime bugs tests missed: kill false-success, SSRF encoded-loopback bypass, SSRF QA-coverage gap, domain-`*` footgun. Lesson: drive the real CLI; mirror `*` across sibling allowlists.

## Open backlog (next session — owner's pick)
- WL-050 token-light progressive-disclosure MCP refactor (ADR-0003) — the natural next mission card.
- WL-051 token-light invariant + budget gate; WL-052 full multi-surface parity.
- WL-053 capture mux socket in peer target (P2, found by /verify).
- Two standing process fixes the owner was offered: (a) deny git/gh to subagents; (b) CI concurrency fix.

## verify_on_resume
- `git fetch origin && [ "$(git rev-parse origin/master)" = "$(git rev-parse origin/develop)" ] && echo converged`  # expect converged @ 1a9bc1f or later
- `git status --porcelain` empty
- `cargo test --all-targets` (default sqlite) and `cargo test --no-default-features --features libsql`

resume_command: /session-relay resume   (reads this packet; weave's own session-relay skill)
