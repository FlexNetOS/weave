# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Harness: weave development

**Goal:** Take any weave change from request to a verified, invariant-clean, drift-free diff via a 4-agent team (plan → implement → verify → guard).

**Trigger:** For any weave code task — feature, bug fix, mux adapter, `Store`/backend change, MCP tool, CLI subcommand, injector change, or a follow-up ("re-run", "update", "redo only the X") — use the `weave-orchestrator` skill. Simple doc/read-only questions may be answered directly. The orchestrator runs the mandatory new-worktree + Rust-native drift checks at the start of every run.

**Change history:**
| Date | Change | Target | Reason |
|------|--------|--------|--------|
| 2026-06-04 | Initial harness build | All agents/skills | - |
| 2026-06-04 | Removed ECC auto-generated `weave` skill | `.claude/skills/weave/` | Misinformation drift: it falsely claimed camelCase filenames, relative imports, `*.test.*` files, freeform commits — none true of this repo |
| 2026-06-04 | Confirmed 4-agent team (kept `weave-guardian` separate from `weave-verifier`) | Team composition | Phase 7 feedback: user opted to keep the dedicated guardian for invariant + Rust-native drift review rather than fold it into the verifier |
| 2026-06-11 | Documented the multi-crate Cargo workspace (`weave-core` ← `weave-inject` ← `weave-mcp` ← `weave`) as the **current** structure; synced the architecture + "Where things live" sections to it | CLAUDE.md, ARCHITECTURE.md | The loop's `WL-001 workspace split` carved the original single crate into four **without updating the docs** (silent structural drift); the split was *not* required to port repowire. **Single-crate remains the goal** — the workspace is kept as an *interim* state and will be collapsed back **after the meta workspace is aligned**; the `backup/*` tags on origin are retained for that collapse. Docs match today's code; meanwhile do **not** add further crates. |
| 2026-06-13 | Adopted the full `.handoff` continuity kernel (migrated `_workspace/`, `sessions-handoff/`, root handoff docs → `.handoff/`; rewired the loop harness; full Tier-B design) and **corrected the branch model** to the owner's actual workflow: **PR target is `develop`** (not `master`), gates-green auto-merge, then `develop` syncs to the protected `master`/`main` via org automation (pending build-out) | CLAUDE.md, `.handoff/` | The documented model said "PR into `master`; `develop` never ahead" — the reverse of the owner's real pattern; `develop` is the integration/PR target and `master`/`main` the protected default it propagates into. Continuity state now lives only under `.handoff/` (P7 / handoff ADR-0003/0004). |

## What weave is

A single static **Rust** binary that lets coding-agent sessions message each other over a shared SQLite mailbox and **push** a message into a recipient's live terminal pane via a native multi-mux injector (tmux, zellij, kitty, wezterm, screen). No Python, no daemon, no runtime dependency on `repowire`. The DB file *is* the broker.

Today the binary is assembled from a **small internal Cargo workspace** — `weave-core` ← `weave-inject` ← `weave-mcp` ← `weave` (bin), with no upward deps — that still links to **one** dependency-light static binary (`target/release/weave`). This workspace is an **interim** structure (an autonomous loop introduced it as `WL-001`): the layering it compiler-enforces is a nice property, but **single-crate is still the goal** — the crates will be collapsed back once the meta workspace is aligned (the `backup/*` tags on origin support that). Until then, work *within* the four crates and do **not** add new ones.

Deep design lives in `ARCHITECTURE.md`; contributor rules in `CONTRIBUTING.md`; test strategy in `docs/TESTING.md`. Read those before non-trivial changes — this file is the operating contract, not a duplicate of them.

## Mandatory session-start ritual

**Start every session in a fresh git worktree, branched off the freshly-fetched `develop`** — never work directly on a shared checkout, and never branch off a possibly-stale *local* ref. weave is a member of the FlexNetOS **meta** workspace, so follow the **meta git worktree policy**: isolate every session in its own worktree off the latest `origin/develop`, keep it inside the workspace, and remove it once merged.

```bash
git fetch origin                                                       # never branch off outdated local refs
git worktree add ../weave-<task-slug> -b <task-branch> origin/develop  # isolate this session on the latest base
# cross-repo work spanning multiple meta members: `meta git worktree add <set>` manages an isolated
# worktree set across the workspace (see the meta:meta-worktree skill / meta-workspace-discipline rule).
```

Then operate inside that worktree. This keeps concurrent agent sessions from colliding on the working tree (weave's whole reason to exist is multi-session work) and keeps each session's diff reviewable in isolation. Remove the worktree when the branch is merged (`git worktree remove`).

**Branch model (`develop` is the integration branch; `master`/`main` is the protected default):**
- **`develop`** is the **PR target** and the always-current base sessions branch their worktrees from. Open **every PR into `develop`** — never into `master` directly.
- **Gates green → auto-merge.** Arm `gh pr merge <n> --auto --squash`; the PR merges once CI is green. The six checks — `rustfmt`, `clippy`, `test`, `build (libsql backend)`, `sign`, `libsql + sign` — run on the PR and are the gate.
- **`master`/`main`** is the protected default branch; **`develop` syncs into it** via the `sync-master` workflow (`.github/workflows/sync-master.yml`): on every push to `develop` it waits for the six required checks to go green on the develop tip, then fast-forwards `master` to it (no-downgrade ancestor guard; refuses a diverged master). Do not push `master` directly.
- **Flow:** worktree off `origin/develop` → work → PR **into `develop`** → auto-merge on green → `develop` syncs to the protected `master`/`main`.

## CRITICAL: keep weave Rust-native — guard against language drift

weave's core invariant is **one dependency-light Rust binary** — today assembled from an *interim* internal workspace (`weave-core`/`weave-inject`/`weave-mcp`/`weave`; single-crate is the deferred goal), still *one* static binary with no extra runtime deps. This repo is also wired to external agent tooling (`ecc-tools`, the `.codex/`, `.agents/`, `.claude/` bundles, and the `handoff/` framework) that **auto-generates and auto-pushes config/package artifacts** — `.codex/*.toml`, `.agents/**/*.yaml`, `.claude/*.json`, `handoff/**` (YAML/JSON), and potentially new files in other languages or formats (e.g. an `.omc` artifact or an ecc-pushed package).

On **every session start, verify there has been no drift away from Rust-native**, and treat any drift as a critical concern to fix:

1. **Scan for non-Rust source/build intrusions.** Anything that becomes part of the *build or runtime* of weave must be Rust. Auto-generated agent-config files (the bundles above) are fine as *sidecar metadata*, but they must never:
   - introduce a build step in another language,
   - add a non-Rust package/dependency to the shippable binary, or
   - become a source of truth that Rust code is expected to mirror by hand.
2. **If drift is found, verify it first** (don't assume — confirm the file actually feeds the build/runtime, e.g. referenced by `Cargo.toml`, `build.rs`, CI, or `src/`). A generated sidecar that nothing builds against is not drift.
3. **If it is real drift, transform it to Rust-native** — port the logic into the appropriate crate/module behind the existing abstractions (`weave-core` for `Store`/`model`, `weave-inject` for `Mux`/`Target`, `weave-mcp` for the MCP tool table) — **and sync it properly with the codebase**: update `Cargo.toml`, both backend builds, tests, and the docs (`ARCHITECTURE.md` / `CONTRIBUTING.md` / `docs/TESTING.md`) in the same change. No silent forks of behavior between a generated artifact and the Rust implementation.
4. **No new heavyweight dependencies in the default build.** Date/time is handled without a date crate on purpose. Anything pulling `tokio` or a large tree belongs behind a feature flag (as `libsql` is). Adding a dep is a deliberate, justified decision — note why it's unavoidable.

## Build / test / lint / format — the full gate

Default build uses **no extra features** and must stay green. Run all four before any PR:

```bash
cargo build --release                    # -> target/release/weave (strip + LTO)
cargo test --all-targets                 # unit + integration + security + prop (default sqlite backend)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check                  # CI form; use `cargo fmt --all` to apply
```

**Run a single test:** `cargo test <name_substring>` (e.g. `cargo test tmux_is_paste_safe`), or `cargo test --test integration <name>` to scope to one integration file. Integration/security/prop tests drive the **compiled binary** (`CARGO_BIN_EXE_weave`) with a scrubbed env and a unique temp `WEAVE_DB`, so they are parallel-safe and never touch the real store.

### Dual backend — both must stay green

The two storage backends are **mutually exclusive** (each statically links its own SQLite C core; a `compile_error!` in `weave/src/main.rs` guards against enabling both). Backend/feature flags (`sqlite` default, `libsql`, `sign`, `llm`) are defined in `weave-core` and re-exported by the `weave` bin crate, so the commands below run from the workspace root. If you touch anything in the `Store` trait or its implementations, build/lint/**test** the libSQL backend too — CI gates it as a blocking job:

```bash
cargo clippy --no-default-features --features libsql -- -D warnings
cargo build  --no-default-features --features libsql
cargo test   --no-default-features --features libsql      # full black-box suite, libSQL backend
```

The `sign` feature (ed25519 message signing) is **also** CI-gated — alone (`cargo clippy/test --features sign`) and combined with libSQL (`cargo … --no-default-features --features "libsql sign"`). `cargo bench` runs the criterion harness (`weave/benches/weave_bench.rs`).

## Architecture in one screen

A small Cargo **workspace** that links to one binary; strictly layered **crates** — each depends only on the crates below it, so "never add an upward dep" is **compiler-enforced**, not just convention:

```
weave (bin)        main.rs    clap CLI + glue; resolve_me() = explicit flag > $WEAVE_SESSION > basename(cwd)
                   setup.rs   `weave setup`/`uninstall`: MCP register + idempotent settings.json hook merge; `resolve_setup_exe` pins a STABLE binary path (never an ephemeral target/.worktrees build) — override with `--exe <PATH>`
                   harness.rs `weave harness ide-merge-ide`: dry-run/execute the Codex 7-layer loop
                   git.rs     worktree / cwd-derived session git tagging
  └ weave-mcp      mcp.rs     MCP stdio JSON-RPC 2.0 server (weave_* tools); live nudge-inject on send
                   http.rs    optional HTTP surface
      └ weave-inject  inject.rs  native multi-mux injector: pure command tables (commands_for) + runner
          └ weave-core  model.rs         core types, no I/O (Message, Peer, now(), fmt_ts, BROADCAST)
                        store.rs          Store trait + bundled SqliteStore (default) + schema
                        store_libsql.rs   feature-gated libSQL/Turso backend (cfg(feature="libsql"))
                        memory.rs         agent memory store
                        config.rs         config.toml + env overlay (WEAVE_*)
                        sign.rs / llm.rs  features `sign` (ed25519) / `llm` (thread summarization)
```

Key mental models:
- **No-daemon push.** There is no relay process. Every mux CLI can target an arbitrary pane/session from any process, so the **sender injects directly** into the recipient's registered pane. The `peers` table maps `name → (mux, pane/session id)`, captured from env at `SessionStart`.
- **Two delivery channels compose.** An injectable peer gets an instant live nudge *and* the full message on its next hook drain; a non-injectable session gets only the hook drain (graceful degradation). Broadcasts are never injected — only persisted and fanned out per-reader.
- **Per-reader read tracking.** Read state lives in `reads(message_id, reader)`, not a flag on the message, so a broadcast is delivered exactly once *per reader*.
- **`commands_for(target, text)` is a pure function** returning exact argv vectors — that purity is what makes the injector unit-testable with no mux installed. Submission is **paste-safe per mux** (TUIs run in bracketed-paste mode; a naive Enter can be swallowed or read as a cancel — the documented `repowire` bug this design fixes).

## Non-negotiable invariants (these are security/correctness, not style)

- **No shell, ever.** Spawn external programs with `std::process::Command::new(bin)` + an explicit argv vector. Never build a command string, never `sh -c`. User text (message bodies, session names) must never reach a shell. (ARCHITECTURE §7)
- **Parameterize all SQL** with bound `params!`. The *only* inlined SQL literals are the broadcast aliases, derived at compile time from `model::BROADCAST` — a drift guard test asserts `BROADCAST_SQL` stays byte-identical so the Rust check and the `recipient IN (...)` filter can't diverge.
- **stdout discipline in MCP.** Only JSON-RPC protocol frames go to stdout; **all logging goes to stderr**.
- **Destructive ops are gated.** `weave_clear {scope:"all"}` requires explicit `confirm:true`; default scope only marks the caller's own inbox read.
- **Input is capped.** Identity length (`MAX_IDENT_LEN`), body length (`MAX_BODY`, 65536), inject body (`MAX_INJECT_CHARS`, 240, truncated on a UTF-8 boundary). `id_valid` rejects malicious target ids.
- **token-light MCP surface (WL-051 / ADR-0003).** The **standing** `tools/list` surface is the single `weave` meta-tool (progressive disclosure), not a flat per-op table — and its serialized size is budget-gated by `MAX_STANDING_TOOLS_BYTES` (≈2k tokens), enforced by the `standing_mcp_surface_is_within_token_budget` test. **Adding a capability must not add standing tokens:** expose new ops through the meta-tool's catalog (`tool_catalog()`), *not* as new standing tools. A ballooning standing tool table is the same species of drift as a heavyweight dependency, and is guarded the same way (the budget test + guardian review). The eager-flat mode (`WEAVE_MCP_EAGER=1`) is an explicit opt-in, exempt from the standing budget. Full **CLI parity** is the zero-standing-cost path (`rtk weave …` for compressed output).
- **Add the matching test layer with every change** (see `docs/TESTING.md` §8 checklist): pure logic → unit test in the owning module; CLI flag → `tests/integration.rs`; MCP tool → an `McpServer` test incl. the failure path; injector rule → exact-argv unit test; new invariant → a proptest property; security/resource property → `tests/security.rs`.

## Where things live

| Want to change… | Edit |
|---|---|
| Message/Peer shape, timestamp/broadcast helpers | `weave-core/src/model.rs` |
| Schema, queries, a `Store` method/backend | `weave-core/src/store.rs` (and mirror in `weave-core/src/store_libsql.rs`) |
| Agent memory store | `weave-core/src/memory.rs` |
| A mux adapter or submission idiom | `weave-inject/src/inject.rs` (then update injector tables in README/ARCHITECTURE) |
| An MCP tool or its JSON schema | `weave-mcp/src/mcp.rs` |
| A CLI subcommand or hook behavior | `weave/src/main.rs` |
| The `weave harness` orchestration surface | `weave/src/harness.rs` |
| Config keys / env overlay | `weave-core/src/config.rs` |

Changing a `Store` method signature means updating **every** backend so both the default and `--features libsql` builds compile.

## Commits

Conventional Commits: `<type>(<scope>): <summary>`, imperative, ≤~72 chars. Scopes mirror modules (`inject`, `store`, `mcp`, `cli`, `config`, `model`). Update `CHANGELOG.md` under `[Unreleased]` for user-facing changes, and the relevant doc (README / ARCHITECTURE) in the same PR.
