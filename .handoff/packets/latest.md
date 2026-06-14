# HANDOFF — weave (weave-loop: resume + /verify + WL-035/036/037 batch)

closed_utc: 2026-06-14T03:10Z
branch: develop @ c366582 (master 306dcd3 syncing forward via sync-master — wait for it to converge)
worktree: main checkout /home/drdave/Desktop/meta/weave (ALL cycle worktrees pruned; local branches = develop + master only)
cycle_budget: 3 (this session ran 4 — owner overrode with an interactive "/verify … implement the next 3 tasks")
cycles_this_session: WL-034, WL-035, WL-036, WL-037 (+ /verify pass + GAP-2 fix)
cycles_total: 44
last_item: WL-037 (supersede chains) — merged #91
next_item: WL-038 (ephemeral messages with TTL + auto-sweep, atm-core parity) — first open mechanical backlog item
orchestrator_phase: complete (plan→implement→verify→guardian ran for WL-034 and the WL-035/036/037 batch)
gate_status: PASS — combined batch GREEN: 626 sqlite / 581 libsql passed, clippy clean under -D warnings on sqlite+libsql+surfaces+sign+libsql·sign, fmt clean
pr_url: (none open) — PRs #90, #91 both MERGED

## Landed this session (all merged to develop)
- #90 feat(export): WL-034 self-contained mailbox HTML export (`weave export`)
- #91 feat: WL-035 backup/restore + WL-036 post-send hooks + WL-037 supersede chains (4-commit batch) + GAP-2 export-write error-context fix

## State (verified)
- develop @ **c366582**, working tree clean, 0 open PRs, ONE worktree (main), branches = {develop, master}.
- master (306dcd3) trails develop — **sync-master** is propagating the #91 batch; it FFs master once the six required checks are green on the develop tip. Do NOT push master directly; just let it converge (verify with the converged check below).
- Pruned this session: 5 stale worktrees (weave-batch/-ci-concurrency/-handoff-ckpt/-hf2/-wl052a-dash) + 16 merged local branches (all had MERGED PRs #63–#85; squash-merge artifacts, not orphaned work). 19 merged branches still exist remote-only on origin (optional GitHub-side cleanup; left deliberately).

## What WL-034/035/036/037 shipped (so the next session doesn't re-investigate)
- **WL-034 export**: `weave export --out <p> [--for <id>] [--limit N]` → self-contained offline XSS-safe HTML, client-side search. Pure `render_mailbox_html` + CENTRALIZED `html_escape` in `weave-core/src/export.rs` (dashboard reuses it). XSS hinge: JSON in `<script type=application/json>` with `</`→`<\/`, rendered via textContent. Reuses `Store::history` (per-identity). WL-034b filed = whole-DB cross-identity export (needs new dual-backend `all_messages()` + a privacy decision) — DEFERRED.
- **WL-035 backup/restore**: `weave backup`/`weave restore` → hand-rolled uncompressed USTAR tar (`weave-core/src/archive.rs`, ZERO new deps) of a `VACUUM INTO` snapshot (`Store::snapshot_to`, both backends; remote libSQL bails) + config + settings.json + MANIFEST. Read-back-verified both ends; `safe_entry_name` traversal guard; `--force`-gated (+`.bak`). Restore note: run `weave setup` after to re-register MCP.
- **WL-036 post-send hooks**: config `[[post_send_hook]]` → `weave-inject::fire_post_send_hooks` (one shared helper) fired from CLI+MCP send/notify/ack. NO-SHELL argv-only; `argv[0]` trusted-dir-constrained; message fields as `WEAVE_HOOK_*` env ONLY (body never exported); pure `*`/exact/BROADCAST matcher; caps + timeout + fault-isolated. Guardian called the spawn "airtight."
- **WL-037 supersede**: `weave send --supersedes <id>` + `weave_send` catalog property (zero standing-token). Additive nullable `messages.superseded_by` (both backends, guarded migration); `Store::supersede` post-stamp; **sender-only authz** (rejects cross-identity censorship). Read: hidden from unread/nudge, retained+flagged in history/thread/search/export. libsql positional projection = trailing col, mapper index 10.

## Dead-ends / hazards (do not re-trip)
- **Guardian BLOCKs on docs-sync every cycle** (WL-034 and the batch both): weave-implementer subagents ship clean code+tests but defer docs → guardian blocks on the code↔docs fork → costs a round-trip. FIX FORWARD: put the exact doc entries (CHANGELOG/README/ARCHITECTURE/SECURITY/PARITY) IN the implementer prompt with the code. (Saved to file-memory `guardian-docs-block-pattern.md` + ICM.)
- **Agent self-delivery hazard** (standing): weave-* subagents were told NOT to git push/commit/gh; the LEADER owns delivery and diff-math-checks before push. Held this session (no sneak-pushes).
- **/verify is worth running on shipped features**: it found GAP-2 (bare os-error) that tests missed, and DISPROVED two false-alarm "gaps" (the raw `</script>` was weave's own structural boundary; `--for a/../../etc` is consistent freeform-id behavior, safe via bound SQL). Drive the REAL CLI, render HTML in headless Chrome.
- **squash-merge ancestry**: `git merge-base --is-ancestor branch develop` returns false for squash-merged branches — use `gh pr list --head <b> --state all` (merged?) + `git cherry develop <b>` to classify, NOT ancestry.

## Open backlog (next session — mechanical order)
- **WL-038** ephemeral messages w/ TTL + auto-sweep (atm-core parity) — NEXT.
- WL-039 idle-notification dedup; WL-040 session export/import (casr); WL-041 read-back verify (casr — partly satisfied by WL-035's pattern); WL-042 multi-provider hook templates (casr).
- **WL-044 Resolve 5 Dependabot vulns (1 high, P1)** — standing security debt, owner-flagged; not mechanical-order but P1.
- WL-045 refresh README "Status" (P2, stale v0.1.0 numbers). WL-043 single-crate collapse (P1, DEFERRED until meta workspace aligned). WL-034b whole-DB export. WL-052b bot command grammar.

## icm_stored
- context-weave 01KV1TXFD4… (WL-034), 01KV20Z9FQ… (verify + WL-035/036/037 batch). file-memory: guardian-docs-block-pattern.md added.

## verify_on_resume
- `git fetch origin && [ "$(git rev-parse origin/master)" = "$(git rev-parse origin/develop)" ] && echo converged`  # expect converged once sync-master FFs master to c366582+
- `git status --porcelain` empty; `git worktree list` = main only
- `cargo test --all-targets` (default sqlite) and `cargo test --no-default-features --features libsql` — expect ~626 sqlite / ~581 libsql green

resume_command: /weave-loop resume   (reads this packet; jumps to WL-038)
