# CLAUDE.md

Guidance for Claude Code when working in this repo.

`weave` is a pure-Rust cross-session messaging mesh (the `weave` CLI + hooks:
`weave_send`/`weave_inbox`/`weave_reply`/`weave_peers`, and the `session`/`stop`/`prompt`
lifecycle hooks). It is the coordination substrate the harness loops use for cross-identity
heartbeats during session handoff.

## Build / test

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

## Harness: autonomous / resumable operation (upgrade path)

This repo's harness can be upgraded to **autonomous, resumable, self-restarting** operation:
a durable on-disk backlog → one item per cycle → hand off to a fresh session at a cycle budget
→ optional fully-unattended self-restart with a clean context each cycle ("/new" effect). Truth
lives on disk (backlog + checkpoints + commits) so any restart resumes cold with zero loss.

- Generic pattern + templates: `~/Desktop/meta/HARNESS-UPGRADE-KIT.md`
- Tailored kit for THIS repo:  `~/Desktop/meta/harness_hub/upgrade-kits/weave.md`
- Note: weave IS the relay substrate — dogfood it. The loop's handoff heartbeat
  (`relay:handoff`/`relay:resumed`, `to:"all"`) goes over weave itself; the committed
  `_workspace/HANDOFF.md` remains the authoritative resume signal.
