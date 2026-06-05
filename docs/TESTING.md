# Testing & Verification Strategy

weave is a single static binary that other agents trust with their messages and,
through the injector, with *typing into a live terminal pane*. That trust budget
is what the test suite is built to protect. Every layer that can be exercised
in-process is unit-tested; every layer that only exists once the binary is
assembled (arg parsing, the MCP stdio protocol, the injector reaching a real
mux) is exercised black-box against the compiled binary; and the routing /
read-tracking / body-fidelity invariants that must hold for *any* input are
checked with property-based tests.

This document describes what is tested, how the suite runs on **both** storage
backends, the fake-mux harness, what proptest caught, and a checklist for adding
tests alongside a new feature.

## Test layers at a glance

| Layer | Location | Style | What it pins down |
|-------|----------|-------|-------------------|
| Unit — pure logic | `src/*.rs` `#[cfg(test)]` modules | in-process | store semantics, injector argv shaping, model/broadcast invariants, config template, hook-command matcher |
| Integration — end-to-end | `tests/integration.rs` | black-box binary | MCP stdio protocol, CLI roundtrips, lifecycle hooks, `--json`/`doctor`/`gc`, backend selection |
| Security / hardening | `tests/security.rs` | black-box binary | flag-injection resistance, destructive-op guard, resource caps, at-rest file mode |
| Property-based | `tests/prop.rs` | black-box + `proptest` | routing correctness, read-tracking idempotence, unicode/long-body roundtrip |
| Benchmark | `benches/weave_bench.rs` | `criterion`, black-box | cold-start, send→inbox roundtrip, inbox-JSON parse throughput |

Run everything (default sqlite backend):

```bash
cargo test --all-targets
```

CI additionally gates `cargo fmt --all --check` and
`cargo clippy --all-targets -- -D warnings` (see `.github/workflows/ci.yml`).

## 1. Unit tests (in-process)

These live next to the code they cover and need no subprocess. They are the
fastest layer and carry most of the injector and store coverage.

- **`src/store.rs`** (`#[cfg(all(test, feature = "sqlite"))]`) — the bundled
  `SqliteStore` against a real on-disk temp DB: send + per-reader read tracking,
  peer upsert/presence (`is_online` / `ONLINE_TTL_SECS`), scoped history,
  `clamp_limit` bounds (a *negative* limit must not become SQLite's unbounded
  `LIMIT -1`), `gc` (deletes old, keeps new), reply auto-addressing + `Re:`
  prefixing + `in_reply_to` linking, transitive `thread` collection, read
  `receipts`, `touch_peer` (refresh without clobbering mux/target/cwd), and the
  `0600` DB file mode. It also covers the presence and federation layers:
  `register_peer_full` round-trips `pid`/`host` (and the 5-arg `register_peer`
  default forwards `pid:None`/`host:""`); a legacy DB without the `pid`/`host`
  columns migrates in place; the `is_alive` matrix (local-dead-PID ⇒ offline,
  remote-host / NULL-pid ⇒ fail-open alive, stale `last_seen` ⇒ offline) and
  `pid_alive` (own PID alive, absurd PID dead on Linux, cfg-degrade otherwise);
  and the federation surface — `open_readonly` reads but cannot write (and never
  creates a missing file), `federated_peers` unions local + foreign and isolates a
  bad store, plus the pure `merge_peer_views` / `merge_session_views`
  dedup/tie-break (these live in `store::federation_tests`, which is **not**
  feature-gated, so they run under both backends). The Tier-2 store layer is
  covered here too: `enqueue_intent` + `list_outbox` roundtrip and cap enforcement;
  `pull_cursor` default/roundtrip; `pull_from_store` commits once and leaves the
  source **byte-unchanged** (owner-only-writes) and is idempotent on re-pull; the
  per-source cursor is duplicate-free on a clean re-drain and bounded to **exactly
  one** intent across a simulated crash-before-advance; a misaddressed / bad-source
  intent is skipped without wedging the source; a legacy (pre-Tier-2) DB gains the
  `outbox` / `pull_cursor` / `keys` tables on open. The signed-identity store layer
  (gated `#[cfg(feature = "sign")]`) covers the `keys` register/get/list roundtrip
  and `signed_pull_verifies_commits_and_rejects_forgery` (valid signature commits;
  a forged signature is **always** rejected; an unsigned intent commits advisory but
  is dropped under `strict_verify`). Because the store-internals
  tests touch backend internals (`s.conn.execute`, the concrete `SqliteStore`),
  that module is gated to the `sqlite` feature — see the dual-backend note below
  for how the *same* behaviors (including the mirrored `register_peer_full`,
  migration, `is_alive` matrix, and the libSQL `open_readonly` read-only proof)
  are still verified under libSQL.

- **`src/inject.rs`** (23 tests) — `commands_for` / `commands_for_mode` are
  **pure** functions returning the exact argv table for each mux, so the entire
  injector shaping layer is asserted byte-for-byte without ever launching a
  terminal: tmux bracketed-paste close (`ESC[201~`) before Enter, zellij CR,
  kitty `--match id:` (and `--to <socket>` ordering when `KITTY_LISTEN_ON` is
  set), wezterm `--no-paste`, screen `stuff`. Plus the hardening rules:
  leading-dash bodies land as content behind an end-of-options `--`; empty /
  whitespace-only text injects nothing (no stray Enter); interior newlines
  collapse to one line; control chars are stripped; oversized bodies cap at
  `MAX_INJECT_CHARS` (240) on a UTF-8 boundary with a `…` marker; `Nudge::Nudge`
  never leaks the body; liveness probes shape per backend and are fail-open for
  unprobed backends; and **`id_valid` rejects malicious target ids**
  (`%3; rm -rf /`, `--listen-on=evil`, embedded spaces) — the injector
  target-validation guard. The pure `capability()` verdict has its own truth table:
  `mux=none` / empty id ⇒ `NotInjectable`; an injectable + unprobed backend ⇒
  **fail-open `Live`** (never a false `RegisteredNotAlive`) — the verdict that
  backs `weave connect` / `weave_connect`.

- **`src/model.rs`** — the broadcast alias set is exposed as both a Rust check
  (`is_broadcast`) and a SQL literal (`BROADCAST_SQL`). A drift guard asserts the
  hand-maintained literal stays byte-identical to the fragment derived from
  `BROADCAST`, so the Rust path and the `recipient IN (...)` delivery filter can
  never disagree.

- **`src/config.rs`** — the `config.toml` scaffold parses as valid (all-default)
  TOML, documents every nudge placeholder, and mentions every real config field
  (including the federation `peer_dbs` and the Tier-2 `pull_from` / `inject_pulled`
  / `allow_inject_from` / `strict_verify` keys) so a newly-added field can't be
  left undocumented. `peer_db_paths()` is unit-tested for parse (comma / path-list
  separator), NUL-entry rejection, dropping the local `db_path()` (no
  self-federation), order-preserving dedup, and the `MAX_PEER_DBS` (16) cap; the
  default (no env, no key) resolves to an empty list (identical-to-today).
  `pull_from_paths()` mirrors that discipline against the **distinct** `pull_from`
  list (a `peer_dbs`-only store is **not** a delivery source) capped at
  `MAX_PULL_FROM` (16); `inject_pulled()` defaults **on** and honors the toggle;
  `strict_verify()` defaults **off** and honors the toggle; `inject_allowed_from()`
  gates to the configured subset (unset ⇒ every pull source eligible; explicit-empty
  ⇒ none).

- **`src/sign.rs`** (gated `#[cfg(feature = "sign")]`) — the optional Ed25519
  module: sign↔verify round-trip + tamper detection, wrong-key rejection,
  malformed-input-is-`false`-not-error, canonical-encoding unambiguity/stability
  (length-prefixed, so `("ab","c")` ≠ `("a","bc")`), the hex codec, `check_pubkey`
  bounds, and a **proptest** property (any `(from, to, body)` verifies; any
  single-field mutation fails). These compile out of the default/libsql builds.

- **`src/setup.rs`** — `is_weave_command` matches only real installed
  `weave hook <event>` lines (and absolute-path forms) while rejecting
  look-alikes (`myweave …`, `weave mcp`, an un-installed event), so
  `setup`/`uninstall` only touch weave's own `settings.json` entries.

## 2. Integration tests — black-box binary (`tests/integration.rs`)

Everything here drives the **built** binary through `std::process::Command`; the
path is resolved at compile time via `CARGO_BIN_EXE_weave`. Each test gets its
own unique temp `WEAVE_DB` (pid + monotonic counter + nanos) and a *scrubbed*
environment — every `WEAVE_*` and mux-detection var (`TMUX_PANE`,
`ZELLIJ_SESSION_NAME`, `WEZTERM_PANE`, `KITTY_WINDOW_ID`, `STY`) is removed and
`XDG_CONFIG_HOME` is pointed at an empty dir — so tests are isolated,
parallel-safe, deterministic, and never read the real store or config. The
`tests/common` module provides these helpers plus a small `McpServer` driver.

Coverage:

- **MCP stdio protocol.** A live `weave mcp` child is spoken to over
  newline-delimited JSON-RPC 2.0: `initialize` returns `serverInfo.name`,
  `notifications/initialized` gets *no* reply, `tools/list` advertises
  `weave_send`/`weave_inbox`/`weave_peers`, a `weave_send`→`weave_inbox`
  roundtrip proves delivery + read tracking, an unknown method returns a proper
  JSON-RPC error object, and closing stdin exits the server cleanly. Reads run on
  a background thread behind a 10s timeout so a wedged binary fails the test
  instead of hanging the suite. The presence/federation tools are covered too:
  `weave_attach` upserts a peer that `weave_peers` then lists, and rejects an
  empty / oversized `me` with `isError`; `weave_connect` returns the live /
  not-injectable verdict with `isError:false` (queued is not an error) and
  `isError:true` only for a non-existent peer; `weave_peers` / `weave_sessions`
  reflect a federated extra store (origin-tagged `(via …)`), and a bad extra store
  mixed in still returns `isError:false` while `weave_doctor` reports the
  extra-store count.

- **CLI roundtrips.** `send`→`inbox` (body delivered, default read consumes, an
  unrelated recipient sees nothing), `register`→`peers` (registered outside a mux
  is `no-inject`), `sessions` reports unread counts, and the
  reply→thread→receipts roundtrip.

- **Lifecycle hooks** — the Claude Code integration. `hook session` registers a
  peer from the payload cwd basename; `hook prompt` drains *and* marks read with
  an explicit payload identity; `hook stop` is a non-consuming **peek** (two
  stops re-surface the same message); a *guessed* identity (empty stdin) peeks
  only and warns; and garbage / unknown events are tolerated (exit 0 with a
  stderr warning, not a crash).

- **New CLI surface.** `--json` output for `inbox`/`peers`/`sessions` parses and
  carries the right shape; `doctor --json` reports the compiled-in backend and db
  path; `gc --older-than-secs` reports a count; an unknown `WEAVE_BACKEND` fails
  loudly instead of silently defaulting; an unknown subcommand exits non-zero
  with a clap usage message. `doctor --json` also carries `db_is_default` (false
  under a temp `WEAVE_DB`, with the hint line in the text form; true when the
  resolved path equals `config::default_db_path()`).

- **Presence & live-connect.** `weave attach` under a fake mux flips a `no-inject`
  peer to `injectable` in `peers --json` (zero-restart adoption); `peers --json`
  carries `pid` / `host` / `alive` (a still-live process reads `alive:true`; a
  recent-but-dead local PID reads `online:false`/`alive:false` on Linux, the
  cfg-guarded A2 path); `weave connect --to` prints the `live` /
  `not injectable (mux=none)` verdict strings and **exits 0** for a queued
  (non-injectable) peer, non-zero for a non-existent one.

- **Read-only federation.** With `WEAVE_PEER_DBS` set to a second store, a foreign
  peer/session surfaces in `peers`/`sessions --json` with `origin`=<store label>
  and `foreign:true` (local rows `origin:"local"`/`foreign:false`, pre-Tier-1 keys
  intact); a foreign session's `unread` is **not summed** into the local set; an
  unset `WEAVE_PEER_DBS` is byte-identical local-only output with **no** `(via …)`
  tag; a bad / non-weave extra path is skipped (exit 0, local peer still listed,
  skip note on **stderr only**).

- **Cross-store delivery (Tier-2).** Two temp stores driven by the real binary:
  `send --to-store` writes an intent into the sender's `outbox` (`outbox --json`)
  while the sender's inbox stays empty; the receiver with `WEAVE_PULL_FROM` set
  `pull`s it into its own inbox with a receiver-assigned id and the sender's `from`
  attribution; a second pull commits **0** (idempotent). Dedup is keyed on the
  intent **id, not content** (two same-body intents both deliver); an unlisted
  source delivers nothing; a misaddressed intent (`to` ≠ me) is not committed; a
  missing / junk source is skipped while a good source still delivers; a plain local
  `send` writes **no** outbox row; a cross-store broadcast is rejected. The MCP
  surface mirrors this: `weave_send` with `to_store` returns `isError:false`
  "Queued intent" and `weave_outbox` lists it (broadcast ⇒ `isError`); a
  `weave_inbox` drain with `WEAVE_PULL_FROM` pulls cross-store messages in the same
  call.

- **Consent injection on a pulled message (Tier-2, fake mux).** Default-on fires
  the content-free nudge into the receiver's **own** pane (the body never appears in
  the recorded `send-keys` argv); `WEAVE_INJECT_PULLED=false` ⇒ pure queue-only (no
  `send-keys`, message still delivered); `allow_inject_from` narrows so a pull source
  not in the subset delivers but never keystrokes; a `mux=none` receiver falls open
  to queue-only; an inject failure is non-fatal to delivery; the MCP `weave_inbox`
  drain nudges by default under the fake mux.

- **Signed cross-store identity (Tier-2, `#[cfg(feature = "sign")]`).** Driving the
  built `--features sign` binary with per-actor `XDG_CONFIG_HOME` temp dirs and
  on-disk key files: `weave key gen` (sender) → `weave key add` (register the
  sender's pubkey on the receiver) → signed `send --to-store` → `pull` commits with
  verified `sender` attribution even under `WEAVE_STRICT_VERIFY=1`; `weave key gen`
  never prints the private key.

## 3. Security tests (`tests/security.rs`)

Same black-box harness, focused on the properties that make weave safe to run
between semi-trusted agent sessions:

- **Flag-injection resistance.** A body that *looks* like a CLI flag
  (`--to=victim --body=pwned; rm -rf /`, or `-n -e --peek …`) is delivered
  byte-for-byte and never re-interpreted as an option — neither on the way in
  (clap parse; bodies use `allow_hyphen_values`) nor on the way out (inbox
  render). The assertion compares the exact stored bytes via `inbox --json`.

- **Destructive-op guard.** `weave_clear {scope:"all"}` without `confirm` returns
  an `isError` result *and* leaves the pre-existing message readable — a stray
  call can't wipe the mesh.

- **Resource guards.** A 100k-char identity is rejected by the MCP layer
  (`MAX_IDENT_LEN`) with `isError` rather than persisted; an over-`MAX_BODY`
  (>65536-byte) body is rejected at the store layer (shared by CLI/MCP/hook) with
  a clear "too long" error.

- **At-rest secrecy.** After a send creates the DB, `mode & 0o077 == 0` — message
  bodies never leak to other local users.

- **Own-row-only adoption.** `weave attach --name attacker` cannot overwrite a
  *victim*'s `(mux, target)` — the upsert is keyed to the caller's own resolved
  identity, so the victim's row is byte-for-byte unchanged and the attacker gets
  its own distinct row. An oversized `attach --name` is rejected (`too long`,
  non-zero exit) and persists no row.

- **Host identity is bounded.** A hostile `$HOSTNAME` (embedded newline / tab /
  CR / ESC / bell + a multi-thousand-char run) persists a `host` that is
  `≤ MAX_HOST_LEN` (128) and control-char-free; a control-only `$HOSTNAME`
  sanitizes to empty and falls back to the stable `"local"` label, so
  `this_host()` can never inject an unbounded or control-bearing value into a row.

- **Federation never writes the foreign store.** After a federated `peers` +
  `sessions` + `doctor`, the foreign store's **main DB file is byte-identical**
  (sha256), no rollback journal is created, and the WAL carries no committed write
  — the structural `SQLITE_OPEN_READ_ONLY` guarantee, proven on **both** backends.
  An oversized `WEAVE_PEER_DBS` (1000 entries) does not fan out 1000 opens (the
  `MAX_PEER_DBS` cap holds), and a traversal-style junk path is opened read-only /
  fails to open and is **never created**.

- **Pull never writes the source store (Tier-2 owner-only-writes).** While the
  receiver actively pulls + commits (twice), the source's **main DB is
  byte-identical** (sha256), the WAL is empty/absent, and no rollback journal is
  created — the structural read-only guarantee, proven on **both** backends. A
  non-allow-listed source delivers nothing; an over-`MAX_BODY` body and an oversized
  recipient are rejected **at enqueue** (nothing in the outbox); a hostile
  1000-entry `WEAVE_PULL_FROM` is capped at `MAX_PULL_FROM` (16); a cross-store
  broadcast is refused for all four aliases.

- **Consent inject is hard-gated (Tier-2, fake mux).** Even with
  `WEAVE_INJECT_PULLED=true` (most permissive), a source on `pull_from` but **not**
  inject-listed records **no** `send-keys` while the trusted source does — proving
  the gate, not a broken harness, is the only difference. The committed message
  **body never appears** in any injected argv (paste-safe, content-free).

- **Signed identity tamper / spoof / strict (Tier-2, `#[cfg(feature = "sign")]`).**
  A present-but-invalid signature is **always rejected** (`pulled 0`, inbox empty,
  source byte-unchanged) in both strict and non-strict modes; an intent claiming a
  `from` it was not signed for (spoof) is rejected against the real registered key;
  an unsigned intent is **dropped** under `WEAVE_STRICT_VERIFY=1` and **commits**
  advisory when off; the private key file is `0600` and the secret is never printed.

## 4. Property-based tests (`tests/prop.rs`)

proptest *generates* sequences of sends (a small fixed peer pool + broadcast
aliases, biased toward direct routing) and asserts invariants over the black-box
binary. Case counts are kept small (24–32) because each generated case spawns
several short-lived `weave` processes; `failure_persistence` is disabled so the
suite is hermetic in CI.

- **Property 1 — routing.** For any send sequence, a single `inbox --me R`
  returns *exactly* the messages addressed to `R` (every `* -> R` plus every
  broadcast from someone other than `R`, never `R`'s own broadcast, nothing for
  others) in send order — checked against an in-test oracle for all peers,
  including "received nothing".

- **Property 2 — read-tracking idempotence.** One default read drains the unread
  inbox; any number of further default reads are empty; `--all` always
  re-surfaces the full, order-preserved history; re-reading never resurrects or
  drops messages.

- **Property 3 — unicode / long-body roundtrip.** Arbitrary unicode (multi-byte,
  emoji, scripts) and long (200–400 char) bodies survive
  `send → store → inbox --json` byte-for-byte — JSON decoding handles escaping,
  so any mismatch is real corruption.

### What proptest caught: the leading-hyphen bug

The unicode/flag-shaped generators surfaced a real defect: a body that *started*
with a dash (e.g. `-n…` or `--…`) was being eaten by clap as an unknown flag
instead of delivered as data, so `send`/`reply` failed (or mis-parsed) on
perfectly valid content. The fix is `#[arg(long, allow_hyphen_values = true)]` on
the `body` (and `subject`) arguments in `src/main.rs`; the regression is now
locked down both by the property test and by the explicit verbatim-delivery cases
in `tests/security.rs`. (The mirror concern at the *output* end — a leading-dash
body re-parsed as a flag by the backend mux CLI — is guarded by the injector's
end-of-options `--` and its `leading_dash_body_is_content_not_a_flag` unit test.)

## 5. The fake-mux injector harness

The injector's argv shaping is proven by pure unit tests, but the
"does the binary actually *spawn* the mux and type the body into the right pane?"
path can only be verified end-to-end. `tests/integration.rs` does this **without
a real terminal** by planting a fake `tmux` on `PATH`:

`make_fake_tmux` writes an executable shell script named `tmux` into a temp dir.
The script records its full argv (`"$*"`) to a log file and exits 0:

```sh
#!/bin/sh
printf '%s\n' "$*" >> '<logfile>'
exit 0
```

The test prepends that dir to `PATH`, registers a peer while pretending to live
in tmux pane `%1` (`TMUX_PANE=%1`), then drives:

- `weave send --to p …` — and asserts the log shows a `send-keys -t %1` carrying
  the body, proving the real send path invoked the injector against the right
  pane; and
- `weave inject --to p --text hi` — the explicit-inject fallback, asserting
  `send-keys -t %7 … hi`.

The log is read with a short bounded backoff (`read_log_with_retries`, ~50×20ms)
so the asynchronous write never flakes and never hangs. Because the fake mux only
has to *exist* on `PATH` for `have()`/`peers` to call the target injectable, this
harness also exercises the "injectable tmux peer" listing.

### Hermetic session-tag (scan) testing

The `weave scan` / `weave_scan` tags (repo · branch · worktree id) are tested
**with no real `git` binary and no repo mutation**, mirroring the no-real-terminal
discipline above. The git parsers in `src/git.rs` are pure functions over fixture
strings: `parse_worktree_id_from_gitdir`, `parse_worktree_porcelain`,
`repo_name_from_toplevel`, and `capture_worktree_tags` — the last driven over a
**crafted temp `.git`** (a `.git` *file* holding `gitdir: …/.git/worktrees/<name>`
→ canonical id; a `.git` *directory* → the `(main)` sentinel; no `.git` → empty
tags), all of which run the worktree-id path without spawning `git` at all.

- **Integration (`tests/integration.rs`, hermetic):** a crafted-temp-`.git`-file
  cwd fixture drives the real `.git`-parse path end-to-end (`weave scan` /
  `scan --json` shape, `--repo`/`--branch` filters, and the tags showing in
  `peers --json`, the `sessions` display-join, and `doctor`'s `peers_tagged`) with
  **no real repo**. The one real-git assertion (a `git init` yielding
  `worktree=(main)` + a non-empty repo tag) is **gated** on a trusted-path `git`
  (mirroring `inject::have("git")` / `inject::resolve_trusted`) and skips cleanly
  when git is absent — it is not `#[ignore]`'d.
- **Migration roundtrip (both backends):** a `legacy_db_without_git_tag_columns_…`
  test opens a `peers` table lacking the three tag columns, runs the additive
  guarded migration, and roundtrips a tagged peer through `get_peer` / `list_peers`
  (positions 8/9/10) plus an idempotent re-open — mirrored sqlite + libSQL.
- **Proptest:** `sanitize_tag` totality/idempotence (any input → control-free,
  ≤128, UTF-8-boundary-safe, `sanitize(sanitize(x)) == sanitize(x)`).
- **Security (`tests/security.rs`):** a hostile cwd-derived tag (control chars,
  newlines, `$(rm -rf ~)`, backticks) is bounded + control-stripped + non-fatal to
  registration and never re-emitted verbatim or injected; a `$(touch PWNED)` /
  `;touch PWNED` worktree-id segment never creates the sentinel file (the tag never
  reaches a shell — argv-only `git`).

## 6. Dual-backend testing (sqlite **and** libSQL)

weave ships two mutually-exclusive storage backends — bundled `rusqlite`
(default) and `libsql`/Turso — because each statically links its own SQLite C
core (a `compile_error!` in `main.rs` guards against enabling both). The on-disk
format is libSQL-compatible, and **the full black-box suite runs against both**:

```bash
# default (sqlite)
cargo test --all-targets

# libSQL backend — a separate binary build
cargo test --no-default-features --features libsql
```

The optional **`sign`** feature (Tier-2 signed identity) composes with both
backends, so the gated `sign` tests add two more columns. The crypto-gated unit /
integration / security tests compile **out** of the default and libSQL columns
(integration / security counts grow only when `sign` is enabled), and a headline
drift check confirms `ed25519-dalek` is absent from the **shippable** build graph
(`cargo tree --edges normal`) of both the default and libSQL builds — the crypto
crate appears only under `--features sign`:

```bash
# signed-identity feature, on each backend
cargo test --features sign
cargo test --no-default-features --features "libsql sign"
```

The integration, security, and property suites are backend-agnostic *by
construction*: they drive the compiled binary and assert behavior (delivery,
read-tracking, routing, hardening), so whichever backend was compiled in is the
one under test. `doctor --json` simply reports `sqlite` or `libsql` and the test
accepts either. The same is true of the backend-agnostic unit modules
(`inject`, `model`, `config`, `setup`).

The one backend-specific layer is `src/store.rs`'s unit module, which is gated
`#[cfg(all(test, feature = "sqlite"))]` because it reaches into the concrete
`SqliteStore` (e.g. `s.conn.execute`). Under `--features libsql` those exact
store *semantics* are still covered, just through the black-box CLI/MCP roundtrips
rather than in-process calls. (Roadmap Phase 1 adds a generic
`assert_store_conformance(&dyn Store)` suite so both backends share one
in-process conformance harness.)

Where a store change must be proven *structurally* per backend, the libSQL module
(`src/store_libsql.rs`, `#[cfg(test)]`) carries the **mirror** of the sqlite
store-unit tests: `register_peer_full` round-trips `pid`/`host`, the legacy
`pid`/`host` migration runs in place, the `is_alive` matrix (incl. the remote-host
fail-open / Turso shared-DB case), and the `open_readonly` read-only proof (a write
through the RO handle is engine-rejected and the foreign DB file stays
byte-identical). So both the sqlite count and the libSQL count grow together when
a mirrored store layer is added — the backends differ only in the count of their
own store-unit module, never in covered behavior. The pure `merge_*_views`
federation tests are not feature-gated and run under both.

CI enforces the libSQL column as a real, blocking gate: a dedicated job runs
`cargo clippy --no-default-features --features libsql -- -D warnings` and
`cargo build --no-default-features --features libsql`. Phase 0 of the v0.2
roadmap closed the historical "both backends green" gap — both now implement the
full `Store` trait (reply/thread/receipts/touch_peer/`in_reply_to` + migration).

### Tier-2 v2 — remote (Turso) cross-store pull: hermetic vs. live

A `pull_from`/`peer_dbs` entry may now be a **remote** libSQL/Turso URL
(`libsql://` / `https://` / `wss://` …), not just a local file path. The whole
test surface stays **hermetic — no network in the default suite**:

- **Unit (`config`):** `classify_source` (every scheme → `Remote`, paths →
  `Local`; total/never-panics proptest), `resolve_store_sources` (trim, NUL-reject,
  cap, first-seen order, local canonicalize+dedup, remote dedup-by-URL with
  trailing-slash normalization), `split_source_list` (a `:`-bearing URL is NOT
  shredded by the path-list splitter), token cap + control-char reject, and the
  redacting `Debug` (the token is never in `{:?}`).
- **Unit (`store_libsql`, OWNER-ONLY-WRITES proof, no network):**
  `read_only_handle_traps_every_write_and_leaves_file_unchanged` flags a local-file
  handle `read_only` (the identical flag `open_readonly_remote` sets) and asserts
  **every** write method returns the `guard_writable` `bail!` error (never panics,
  never writes) and the foreign file is byte-identical afterwards. This is the
  unattended proof that weave never writes a foreign/remote store.
- **Integration (default sqlite build):** a remote URL in `WEAVE_PEER_DBS` is
  rejected **loudly** ("requires `--features libsql`") on stderr while local
  sources still succeed; `doctor --json` reports
  `federation_remote_stores`/`federation_remote_unsupported`; the auth token
  (`WEAVE_PULL_TOKEN`) never appears in any output. A liveness regression test
  confirms a foreign-`host` peer is TTL-judged, never pid-probed.
- **Live remote (env-gated, `#[ignore]`, never in CI):**
  `remote_live_pull_delivers_and_is_idempotent` runs **only** when you set the env
  and pass `--ignored`, against a real Turso DB:

  ```bash
  # 1. Create a Turso DB and a READ-ONLY token (the recommended deployment contract):
  #      turso db tokens create <db> --read-only
  # 2. Seed its outbox out-of-band with an intent addressed to `bob`.
  # 3. Run the gated test (built with the libsql backend):
  WEAVE_TEST_TURSO_URL=libsql://<db>.turso.io \
  WEAVE_TEST_TURSO_TOKEN=<read-only-token> \
    cargo test --no-default-features --features libsql -- --ignored remote_live
  ```

  CI sets neither var and never passes `--ignored`, so the default suite stays
  offline and deterministic. Tune the per-call network bound with
  `WEAVE_PULL_TIMEOUT_MS` (default 5000ms; an unreachable remote is just a skip).

#### Per-source pull tokens (`WEAVE_PULL_TOKEN_<LABEL>`) — resolution, not network

Per-source token selection is tested as **resolution + hygiene**, hermetically; the
cases assert which token tier wins and that no token ever leaks — never live auth:

- **Unit (`config`):** `is_valid_label` (charset/bounds + a totality + uppercasing
  proptest), `parse_labeled_source` (a `LABEL=remote-url` splits to `(Some(LABEL),
  Remote)` uppercased; an invalid label, an empty label, or a non-remote right side
  degrades to the verbatim entry — a proptest asserts the no-label result equals
  `classify_source` of the verbatim entry), and `per_source_token` precedence
  (label-env set+sane wins; set-but-over-cap/control-char **falls through** to the
  shared token; unset → shared; neither → none). `resolve_store_sources` proves a
  labelled and an unlabelled remote coexist (per-source vs. shared token) in one
  resolve, and the redacting `Debug` still hides both tokens.
- **Integration (scrubbed env, both backends):** with `WEAVE_PULL_TOKEN_<LABEL>` set
  for labelled remotes and a shared `WEAVE_PULL_TOKEN`, `weave doctor --json`'s
  token-free tier counts (`federation_remote_token_per_source` / `_shared` / `_none`)
  resolve correctly and **none** of the per-source or shared token bytes appear in
  stdout or stderr. These run on the default sqlite build (remote loud-rejected) and
  the libsql build (unreachable host + short timeout, skipped) — resolution and the
  secret-never-printed invariant are asserted, not a real connection.

## 7. Benchmarks (`benches/weave_bench.rs`, criterion)

A `criterion` harness (`harness = false` in `Cargo.toml`) tracks the costs that
matter operationally. It mirrors the test harness (scrubbed env, unique temp DB)
but is standalone so the bench crate has no dependency on the test module tree:

- **`cold_start`** — `weave --version` (pure spawn + clap parse, store never
  opened) vs `weave doctor --json` (spawn + store open + peer query); the delta
  is the store-open cost a hook/agent pays.
- **`send_inbox`** — a full two-process `send` then `inbox --json` roundtrip
  against a fresh per-iteration DB (so inbox size stays constant).
- **`inbox_json_parse`** — a pure in-process `serde_json` parse of a synthetic
  `inbox --json` payload at 100 / 1,000 / 10,000 messages, reported as MiB/s,
  isolating deserialization throughput with no subprocess in the hot loop.

Run with:

```bash
cargo bench
```

## 8. Checklist: adding tests with a new feature

When you add or change a feature, add the matching coverage before you call it
done:

1. **Pure logic → a unit test** in the owning module's `#[cfg(test)]` block.
   Prefer making the core a pure function (like `commands_for`, or the
   `src/git.rs` tag parsers) so it can be asserted with no subprocess. When the
   feature reads an **external binary or filesystem** (e.g. `git`), test the parse
   path hermetically over fixtures / a crafted temp `.git` and gate any real-binary
   assertion on a trusted-path probe (`inject::have` / `inject::resolve_trusted`)
   so the suite passes with the binary absent (see §5).
2. **New CLI subcommand / flag → an integration test** in `tests/integration.rs`
   using the `tests/common` helpers (`run_ok`, `run`, `run_hook`,
   `run_stdin_full`). If it has machine-readable output, assert the `--json`
   shape, not just substrings.
3. **New MCP tool / protocol behavior → an `McpServer` test** (`spawn`,
   `call_tool`, assert `isError` and the returned text). Include the failure path
   (bad/oversized args → `isError`, never a panic or silent persist).
4. **New injector backend or shaping rule → unit tests** in `src/inject.rs`
   asserting the exact argv table, the end-of-options `--` guard for
   leading-dash bodies, the empty/whitespace no-op, and `id_valid` rejection of
   malicious target ids. If a real spawn matters, extend the fake-mux harness.
5. **A new invariant ("for any input, X holds") → a proptest property** in
   `tests/prop.rs`. Keep `cases` small (subprocess-heavy) and
   `failure_persistence: None`.
6. **A security or resource property → a `tests/security.rs` test**: verbatim
   delivery of hostile input, a confirm-gated destructive op, a length/identity
   cap, or a file-permission assertion.
7. **A new config field → extend the `config.rs` template tests** so the field is
   documented and the scaffold still parses.
8. **Touched the store API → keep both backends green.** Run the suite under
   `--no-default-features --features libsql` as well as the default, and make
   sure clippy passes for the libSQL build (CI gates both). If you add a column,
   add the additive migration and a roundtrip test.
   - **Behind an optional feature (e.g. `sign`) → gate the test too** with
     `#[cfg(feature = "...")]` so the default build stays clean, and run the
     feature column on **both** backends (`--features sign`,
     `--no-default-features --features "libsql sign"`). For a new dependency, add a
     drift check that it is absent from the default shippable graph.
9. **Performance-sensitive path → consider a criterion bench** so regressions are
   visible.
10. **Before pushing**, the full gate is: `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, and
    the libSQL clippy/build/test column.
