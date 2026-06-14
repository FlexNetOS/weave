# HANDOFF — weave (weave-loop: WL-038..042 batch + WL-045 + WL-040b)

closed_utc: 2026-06-14T06:18Z
branch: develop @ 38c6836 — **trunks CONVERGED** (origin/master == origin/develop == 38c6836)
worktree: main checkout /home/drdave/Desktop/meta/weave ONLY (all cycle worktrees removed; local branches = develop, master, chore/handoff-2026-06-14 [pre-existing, untouched])
cycles_total: 51
last_item: WL-040b (ask-thread + ask-group replay on import) — merged #96
next_item: **WL-044** (resolve 5 Dependabot vulns: 1 high, 1 moderate, 3 low — P1 standing security debt). Mechanical parity backlog is otherwise DRAINED.
orchestrator_phase: complete (plan -> implement -> verify -> guardian APPROVE for every card)
gate_status: PASS — develop tip green; latest WL-040b gate 717 sqlite / 668 libsql / 708 libsql+sign; clippy -D warnings clean (--all-targets) on sqlite+libsql+sign+surfaces; fmt clean; standing-MCP token budget + BROADCAST drift-guard green
pr_url: (none open) — PRs #93, #95, #96 all MERGED; #94 (stale handoff) CLOSED deliberately

## Landed since the last packet (all merged to develop, master converged)
- **#93** feat WL-038..042 (one batch): ephemeral TTL msgs, idle-notification dedup, canonical session export/import, read-back-verify of destructive config/hook writes, multi-provider `setup --provider`. 706 sqlite / 657 libsql, +91 tests.
- **#95** docs WL-045: README "Status" refreshed to v0.2.0 reality (four-crate workspace, 706/657 tests, token-light MCP surface, live tmux/zellij injection validated, default-OFF zero-dep features). Dropped the stale `v0.1.0 — 38 tests … to be confirmed`.
- **#96** feat WL-040b: faithful ask-thread + ask-many GROUP replay on session import (completes WL-040). 717 sqlite / 668 libsql / 708 libsql+sign, +12 tests.

## What WL-038..042 + WL-040b shipped (so the next session doesn't re-investigate)
- **WL-038 ephemeral TTL**: `weave send --ttl <secs>` + `weave_send` catalog `ttl`. Additive nullable `messages.expires_at` (absolute `ts+ttl`, both backends, TRAILING projection index 11); `set_message_expiry` post-stamp; `MAX_MSG_TTL_SECS=86400` cap. Delete-on-sweep: `sweep_expired_messages` + `gc()` fold-in + opportunistic pre-read sweeps. `outbox.ttl` cross-store carry.
- **WL-039 idle dedup**: opt-in `weave notify --dedup-idle` + `weave_notify` catalog `dedupIdle`. Additive nullable `messages.kind` (both backends, TRAILING index 12; `'idle'` only on notify). New `Store::supersede_prior_idle` reuses the WL-037 `superseded_by` hide-spine, sender-only authz. Test-proven: NEVER touches a real message.
- **WL-040 session export/import**: `weave session export/import` canonical versioned JSON; pure `weave-core/src/session.rs` + `weave/src/session.rs`. Messages + mesh-memory round-trip via `Store::send` reuse (no schema change). Contract: `docs/FORMAT-session-export.md`.
- **WL-040b ask replay (completes WL-040)**: 3 new dual-backend Store methods — `import_ask` (out-of-order materializer: inserts an ask directly in any AskState, bypassing the create->answer->ack lifecycle since the question/answer message rows already exist), `import_ask_group`, `list_ask_groups`. Envelope additively gained `ExportedAsk.{kind,options,reply_to,close_note,parent_id}` + `ExportedAskGroup` + `ask_groups` (NO schema_version bump — additive, back/forward compatible). Import remaps each ask's question/answer msg id to the new local id (resolved by idempotency_key, incl. deduped msgs); dangling ask ref skipped+counted (never a forged link); idempotent re-import (dedup on remapped asker,askee,question_msg_id); `--as` remap. ask_groups COMPLETED (no WL-040c).
- **WL-041 read-back verify**: every destructive config/hook writer re-reads+re-parses+asserts before Ok (`setup.rs` merge/prune/git-hook + `backup.rs` restore). Reusable `verify_settings_*` helpers; never-clobber-foreign. config.rs unchanged (LOAD-ONLY).
- **WL-042 multi-provider**: `weave setup --provider <claude|codex|gemini|aider>` (default claude byte-identical). Rust-native, ZERO new deps (hand-templated codex/gemini/aider configs). gemini/aider = scaffold-with-caveat (documented). Reuses WL-041 helpers.
- **WL-045 README**: see #95 above.

## Dead-ends / hazards (do not re-trip)
- **rust-analyzer false-positive (recurring)**: integration.rs shows "Syntax Error: expected pattern" at `let…else` lines — an OLD rust-analyzer parser bug, NOT a real error. `cargo test --all-targets` + CI's `test` job compile it fine. Trust cargo/CI, not the IDE.
- **JobState package-scoped clippy warning**: `cargo clippy -p weave-core` flags an unused `JobState` import at store.rs:11 (all users sqlite-cfg-gated). It is PRE-EXISTING on develop and CI-INVISIBLE (CI runs `--all-targets` workspace clippy, which is clean). Not a blocker; don't chase it as new.
- **Guardian-docs-block pattern (held all session)**: give implementers the EXACT doc entries (CHANGELOG/README/ARCHITECTURE/PARITY/FORMAT) in their prompt -> docs ship WITH code -> guardian APPROVE first pass. Keep doing this. (file-memory guardian-docs-block-pattern.md)
- **Agent self-delivery hazard (held)**: weave-* subagents do NOT push/commit/gh; the LEADER owns commit/push/PR/auto-merge and resolves rebases. Held all session.
- **Shared-file serialization**: cards touching store.rs/main.rs/setup.rs/session.rs/integration.rs MUST run serial in one worktree (parallel implementers conflict). Planners run parallel (read-only). Each implementer reads prior impl_*.md to build on current state.
- **Three additive trailing message columns now exist** (superseded_by idx10, expires_at idx11, kind idx12): any FUTURE messages column is index 13+ and must be appended to EVERY explicit `SELECT ... FROM messages` projection in BOTH store.rs and store_libsql.rs (libsql positional; sqlite thread CTE positional too). Verifier spot-checks alignment.
- **Merge-train rebases**: when sibling PRs merge first, rebase the open branch onto origin/develop before relying on auto-merge — CHANGELOG `[Unreleased]` is the usual conflict (resolve by keeping all entries under one header). Done for #96 (rebased onto #95).

## Open backlog (mechanical parity DRAINED through WL-042/WL-040b)
- **WL-044 Resolve 5 Dependabot vulns (1 high, 1 moderate, 3 low)** — P1, owner-flagged, NEXT. weave aims dependency-light: review the alerts, bump/replace keeping the default build lean (default build has ZERO non-std deps beyond rusqlite; vulns are likely in optional-feature deps — libsql/reqwest/ed25519 trees). Check `cargo audit` / the GitHub Dependabot tab.
- **WL-043 single-crate collapse** — P1 but DEFERRED until the meta workspace is aligned (backup/* tags retained; do NOT prune).
- **WL-034b** whole-DB cross-identity export (needs `all_messages()` + a privacy decision).
- **WL-052b** bot command grammar (Telegram/Slack structured commands).
- (WL-040c was NOT needed — ask_groups completed in WL-040b.)

## icm_stored
- context-weave 01KV28KZ2D58J7GZHTGZY9C4WS (WL-038..042 batch + the parallel-planners/serial-implementers pattern). WL-045 + WL-040b are doc/code-tracked; store a follow-up if WL-044 surfaces a decision.

## verify_on_resume
- `git fetch origin && git status --porcelain` empty; `git worktree list` = main only; `[ "$(git rev-parse origin/master)" = "$(git rev-parse origin/develop)" ] && echo converged`  # expect 38c6836
- `cargo test --all-targets` (sqlite, expect ~717) && `cargo test --no-default-features --features libsql` (expect ~668)

resume_command: /weave-loop resume   (reads this packet; mechanical parity is drained — next is WL-044 Dependabot, P1)
