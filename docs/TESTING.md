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


## Backlog/docs freshness gate (WL-076)

CI runs:

```bash
python3 scripts/docs_freshness_check.py
```

The gate is intentionally lightweight: if a PR changes operator-visible CLI, MCP,
workflow, or user-facing documentation paths, it must also update `CHANGELOG.md`
or `.handoff/loop/backlog.md`. If the change truly has no release-note/backlog
impact, add `[no backlog/doc change]` to the PR body. The script also supports
`--marker` for local dry runs and `--self-test` for its built-in checks.

```bash
python3 scripts/docs_freshness_check.py --self-test
python3 scripts/docs_freshness_check.py --marker '[no backlog/doc change]'
```

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

  **Host-aware liveness (`liveness_for`).** The pure A2 classifier is exercised by
  a `#[cfg(test)]` matrix that passes a **fixed `this_host` + `now_ts`** — never the
  real hostname or wall clock — so every regime is deterministic: same-host +
  live-pid ⇒ `AliveLocal`; same-host + null-pid + recent ⇒ `AliveLocal`; same-host
  + dead-pid + recent ⇒ `Stale` (Linux-gated, pid authoritative beats recency);
  remote-host + recent + absurd-pid ⇒ `AliveRemote` (proving the remote arm is
  **never pid-probed**); empty-host + recent ⇒ `AliveRemote` (fail open); the TTL
  boundary (`now_ts - ONLINE_TTL_SECS` inclusive-alive vs `-1` stale, matching
  `is_online_at`'s `<=`); and `token()`'s `alive_local`/`alive_remote`/`stale`
  strings. **remote-stale is covered here** at the pure layer (a backdated
  `now_ts`), so the integration layer needs no wall-clock backdate hack. A
  **delegation regression-lock** asserts `liveness_for(p, &this_host(), now()) !=
  Stale` equals `is_alive(p)` over every regime peer, proving the truth table is
  unchanged. A second delegation lock asserts
  `liveness_for(p, this_host, now_ts) == liveness_from_fields(&p.host, p.pid,
  p.last_seen, this_host, now_ts)` over the same fixed-host/now matrix, proving the
  field-level seam the dashboard render uses is byte-identical to the full-`Peer`
  path.
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
  `outbox` / `pull_cursor` / `keys` / `identity_keys` tables on open. The
  **multi-key registry** (#7) is covered by `keys_register_get_list_roundtrip`
  (append/no-op/remove/most-recent-shim semantics), `register_key_enforces_per_identity_cap`
  (the `MAX_KEYS_PER_IDENT` = 16 cap: cap+1 distinct keys errors on the last, a
  duplicate never counts, per-identity isolation holds), and
  `legacy_single_key_migrates_into_identity_keys` — a genuine pre-#7 DB (a single-key
  `keys` row, no `identity_keys`) migrates the row into `identity_keys` on open and a
  SECOND open does **not** duplicate it (the additive, idempotent migration roundtrip).
  The signed-identity store layer (gated `#[cfg(feature = "sign")]`) covers
  `signed_pull_verifies_commits_and_rejects_forgery` (valid signature commits;
  a forged signature is **always** rejected; an unsigned intent commits advisory but
  is dropped under `strict_verify`) and `verify_decision_table_every_cell` — a
  table-driven test exercising **every cell** of `verify_pulled_intent`
  (trusted+good/bad/unsigned, untrusted+good/unsigned, no-trust-set, global strict
  forced/disabled, and R1: a valid signature against a **revoked** key rejected even
  with strict disabled). The multi-key verification core adds
  `multikey_registry_old_and_new_verify_then_revoke_old` (the decision matrix: a sig
  by the OLD or the NEW registered key both COMMIT; after revoking the old fingerprint
  the old key's sig is REJECTED while the new key's still commits; a sig by a THIRD
  unregistered key verifies against neither ⇒ REJECT) and the **rotation-overlap
  E2E** `rotation_overlap_then_revoke_old` (a receiver registers BOTH old + new keys;
  intents signed by either both commit during the window; revoking the old fingerprint
  then drops the old-key intent while the new-key intent still commits, with the
  source DB byte-unchanged — owner-only-writes). `multikey_single_key_is_byte_identical_to_v3`
  is the **regression-lock**: with exactly one registered key, good⇒commit /
  forged⇒reject / revoked-good⇒reject is byte-identical to the #3 single-key model.
  Both seed keys from **fixed bytes** (`SigningKey::from_bytes(&[seed; 32])`, never
  `OsRng`), and ed25519 verify is RNG-free, so they are bit-stable across repeat runs.
  The **observed-revocation audit log** (#11) adds a `record/list/count_revocations`
  roundtrip (most-recent-first ordering, `--limit` clamp, declared/enforced kinds), an
  idempotent-migration case (a pre-#11 DB without `revocations` gains the table on
  open, a second open is a no-op — mirroring the `identity_keys` legacy migration), and
  the **R1-independence** assertion: the revoked-key REJECT decision is unchanged and
  exactly one `enforced` row is recorded on the rejection, proving the best-effort
  audit write never feeds back into `verify_pulled_intent`. The libSQL backend mirrors
  the roundtrip/migration and proves `record_revocation` **traps on a read-only handle**
  (owner-only-writes).
  Because the store-internals
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
  target-validation guard. The capability facts have their own pure truth table:
  `mux=none` / empty id ⇒ `NotInjectable`; an injectable target with no trusted mux
  binary ⇒ `TransportUnavailable` (never a false `Live`); with transport available,
  an unprobed/inconclusive backend remains **fail-open `Live`** — the verdicts that
  back `weave connect` / `weave_connect`. Trusted resolution tests require
  executable mode, then prove with a real launch that a metadata-compatible but
  unlaunchable program still cannot count as live transport.

- **`src/model.rs`** — the broadcast alias set is exposed as both a Rust check
  (`is_broadcast`) and a SQL literal (`BROADCAST_SQL`). A drift guard asserts the
  hand-maintained literal stays byte-identical to the fragment derived from
  `BROADCAST`, so the Rust path and the `recipient IN (...)` delivery filter can
  never disagree.

- **`src/config.rs`** — the `config.toml` scaffold parses as valid (all-default)
  TOML, documents every nudge placeholder, and mentions every real config field
  (including the federation `peer_dbs` and the Tier-2 `pull_from` / `inject_pulled`
  / `allow_inject_from` / `strict_verify` / `trust` / `revoked` keys) so a
  newly-added field can't be left undocumented. `peer_db_paths()` is unit-tested for parse (comma / path-list
  separator), NUL-entry rejection, dropping the local `db_path()` (no
  self-federation), order-preserving dedup, and the `MAX_PEER_DBS` (16) cap; the
  default (no env, no key) resolves to an empty list (identical-to-today).
  `pull_from_paths()` mirrors that discipline against the **distinct** `pull_from`
  list (a `peer_dbs`-only store is **not** a delivery source) capped at
  `MAX_PULL_FROM` (16); `inject_pulled()` defaults **on** and honors the toggle;
  `strict_verify()` defaults **off** and honors the toggle, while the tri-state
  `strict_verify_override()` distinguishes unset / `Some(true)` / `Some(false)`;
  `inject_allowed_from()` gates to the configured subset (unset ⇒ every pull source
  eligible; explicit-empty ⇒ none). `trust_set()` / `revoked_set()` parse the
  comma/whitespace-split fingerprint lists (never on `:`, so `SHA256:` survives),
  reject control chars and over-long entries (`MAX_FP_ENTRY_LEN`), dedup, and cap at
  `MAX_TRUST` (64); `trust_set_configured()` is true only when the validated trust
  set is non-empty.

- **`src/sign.rs`** (gated `#[cfg(feature = "sign")]`) — the optional Ed25519
  module: sign↔verify round-trip + tamper detection, wrong-key rejection,
  malformed-input-is-`false`-not-error, canonical-encoding unambiguity/stability
  (length-prefixed, so `("ab","c")` ≠ `("a","bc")`), the hex codec, `check_pubkey`
  bounds, and a **proptest** property (any `(from, to, body)` verifies; any
  single-field mutation fails). Plus the **fingerprint** layer:
  `fingerprint`/`fingerprint_full` determinism and format (`SHA256:` + 16-hex
  display, 64-hex full digest, lowercase), `None` (never panic) on
  malformed/oversized/non-32-byte input, `fingerprint_matches` matches only the
  **full** digest / full pubkey hex (a truncated display string never matches), and
  a **proptest** `fingerprint_total_and_stable` (for any hex-ish input it returns
  `Some`/`None` without panic and is stable across calls). All crypto tests seed
  keys from **fixed bytes** (a `test_key(seed)` helper), never `OsRng`, so they are
  deterministic across repeat runs. These compile out of the default/libsql builds.

- **`src/setup.rs`** — `is_weave_command` matches only real installed
  `weave hook <event>` lines (and absolute-path forms) while rejecting
  look-alikes (`myweave …`, `weave mcp`, an un-installed event), so
  `setup`/`uninstall` only touch weave's own `settings.json` entries.

### Test env hygiene — the canonical `WEAVE_*` env guard

`std::env::set_var` / `remove_var` mutate **process-global** state, and cargo
runs the unit-test layer **multithreaded** — so any two in-process tests that
touch overlapping `WEAVE_*` vars (one writing while another reads, e.g.
`inject::trusted_dirs` reading `WEAVE_MUX_DIR`) race without a shared lock. The
crate carries **one** canonical serialization guard for this:

- **`crate::testenv`** (`src/testenv.rs`, `#[cfg(test)]`, std-only) — a leaf
  test-support module reachable as `crate::testenv` from every module's
  `#[cfg(test)]` block. It is **not** compiled into the shippable binary.
  - **`lock_env() -> MutexGuard<'static, ()>`** — a poison-tolerant
    `OnceLock<Mutex<()>>` accessor; **all** `WEAVE_*`-touching unit tests
    serialize on this one lock. Poison tolerance means a panicking test surfaces
    as its own failure instead of deadlocking the rest of the suite.
  - **`EnvVarGuard`** — RAII set/remove that records the prior value on
    construction and **restores the exact prior state on Drop** (sets it back, or
    removes it if it was absent), even on panic — so no test leaks env into
    another. Hold `lock_env()` for the guard's whole lifetime: the lock provides
    exclusion, the guard provides restoration.

> **Contributor invariant:** every unit test that **reads or writes a `WEAVE_*`**
> (or any process-global env var) MUST `let _g = crate::testenv::lock_env();` as
> its first statement and mutate via `crate::testenv::EnvVarGuard` — never call
> `set_var`/`remove_var` on a `WEAVE_*` var in a unit test without the lock.
> Integration / security / property tests do **not** need it: they spawn the
> compiled binary as a separate process with a scrubbed env (§2), so they are
> already process-isolated.

The serialization is proven by `env_guard_serializes_concurrent_weave_mux_dir`
(8 threads × 200 iterations, each `lock_env()` + `EnvVarGuard::set` + a
`trusted_dirs()` read), plus `testenv`'s own RAII restore/remove self-tests. The
count is **iteration-bounded, never wall-clock** (the anti-flake rule).

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
  The optional hook responder path is also covered: with
  `WEAVE_RESPONDER_ON_HOOK=1`, `hook notification` performs a quiet one-shot ACK sweep,
  surfaces through `ask-status`, remains idempotent, and never marks the original
  question read or closes the ask.

- **Responder/ACK parity.** CLI `responder` sends idempotent non-closing
  `[weave-ack]` replies, `responder --health --json` reports open/unacknowledged
  counts, MCP `weave_responder` mirrors the one-shot ACK surface, and MCP
  `weave_ask_status` parses and displays auto-ACK status/body just like the CLI.

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

- **Daemon lifecycle (CLI + MCP).** `weave daemon start` spawns a background
  heartbeat process; `weave daemon status` reports `running` with the PID;
  `weave daemon stop` sends `SIGTERM` and cleans up the pidfile. The MCP mirrors
  (`weave_daemon_start|stop|status`) return JSON-shaped text and are tested in
  the same roundtrip: start → status confirms running → stop → status confirms
  stopped. Both use a temp-scoped `WEAVE_PIDFILE` so parallel tests never collide.

- **Scan remote-host surfacing.** Extending the proven foreign-store fixture
  (forced `HOSTNAME` + `WEAVE_PEER_DBS`), a federated peer registered on a
  *different* host with a recent `last_seen` surfaces in `weave scan --json` with
  `remote:true` and `liveness:"alive_remote"` (and `alive:true`) — proving the
  remote row is TTL-judged, never pid-probed across hosts — while the human output
  carries the ` <remote>` marker, the `[alive (remote, ttl)]` reason, and the
  trailing `summary: N local-alive, M remote-alive, K stale` count line matches the
  rows. Because the forced foreign `HOSTNAME` differs from this host, the
  remote-alive case needs **no backdate / wall-clock** seam (remote-stale is locked
  at the pure `liveness_for` layer above, with a fixed `now_ts`). The **same**
  forced-`HOSTNAME` foreign-store fixture drives the parallel surfacing on the
  other three surfaces: `peers --json` carries the additive `liveness` (token) +
  `remote` (bool) keys (and the human row its ` <remote>` marker + `[reason]`);
  `doctor --json` carries `peers_alive_local` / `peers_alive_remote` /
  `peers_stale` (and the human `liveness:` three-count line); and `sessions --watch
  --iterations 1` prints the per-row `[reason]` + the three-count header — one
  liveness vocabulary asserted identically across all four surfaces.

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
  never prints the private key. The tightened-identity layer adds: `weave key
  fingerprint` / `key list --json` print `SHA256:<16-hex>` fingerprints; `weave key
  rotate` produces a new key and keeps the old one verifying during the trusted
  overlap; **default-when-trust-set** — with `WEAVE_TRUST=<sender-full-fp>` and **no**
  `WEAVE_STRICT_VERIFY`, an unsigned intent claiming that trusted sender is
  **rejected** (`pulled 0`) while a signed one commits; and a **no-trust-set
  regression** — with no `WEAVE_TRUST`, an unsigned intent still commits (advisory,
  unchanged).

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
  The tightened model adds: a forged/tampered signature under a configured trust set
  is rejected (`pulled 0`); **R1** — a **revoked** fingerprint's signed message is
  rejected even with `WEAVE_STRICT_VERIFY=false` (revocation is absolute for signed
  messages); and the secret-never-printed assertion is extended across the new
  `key fingerprint` / `rotate` / `revoke` / `key remove` / `doctor` commands (the
  on-disk key hex, and every `.bak` archive from rotate, never substrings any
  command's stdout — including the multi-key `key list` and `key remove` output).
  For the #7 multi-key registry: a key that is revoked **within a multi-key set**
  still cannot verify (R1 holds even when other registered keys exist for the same
  identity), and no private-key bytes appear in any `key list` / `key remove` /
  `doctor` output (only pubkeys, fingerprints, and per-identity counts are surfaced).
  For the #11 revocation audit log: `weave audit revocations` (and `--json`) output is
  **secret-free** (no private-key hex, no full peer pubkey — only `SHA256:` fingerprints,
  public identities/source labels, kinds, and counts) and **bounded** (`--limit` past
  the cap returns ≤ cap rows; an oversized fp/source is clamped at the write seam), and
  the `weave_doctor` / `weave doctor` verify summary surfaces counts + the own
  fingerprint only. The `--features "libsql sign"` column proves the same security
  parity on the alt backend.

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

- **Property 4 — `liveness_for` totality + determinism.** For any
  `(host, this_host, pid, last_seen, now_ts)`, `liveness_for` never panics and is
  **deterministic** (called twice with the same inputs it returns the same
  variant). It is exercised purely (no real hostname/clock, no `/proc`-dependent
  pid in the generator), guarding the A2 classifier's totality and locking that a
  remote arm decision never depends on a probe.

- **Property 5 — ask lifecycle monotonicity.** For all `(from, to)` `AskState`
  pairs and every transition kind, `AskState::can_transition` permits **only** the
  three legal edges (`Open→Answered`, `Open→Acked`, `Answered→Acked`) and **never**
  a backward, self, or from-terminal edge — the pure "an ask never goes backwards"
  invariant. Exercised purely (no store, no subprocess), so it pins the state
  machine independently of the SQL that enforces it.

- **Property 6 — ephemeral expiry monotonicity (WL-038).** For any non-negative
  base `ts` and a valid `ttl in 1..=MAX_MSG_TTL_SECS`,
  `model::expiry_from_ttl(ts, ttl)` is **strictly** after `ts` (no wrap) and bounded
  by `ts + MAX_MSG_TTL_SECS`; a companion case asserts the helper **saturates**
  (`== ts.saturating_add(ttl)`) for arbitrary `i64` inputs and never panics. This
  pins the no-overflow guarantee behind the `expires_at = ts + ttl` deadline that the
  CLI/MCP TTL cap (`ttl_valid`) and the store sweep rely on. The precise
  delete-on-sweep / read-exclusion behavior is covered by the dual-backend store unit
  tests (`expiry_stamps_and_excludes_from_unread`, `sweep_expired_messages_*`,
  `gc_also_reaps_expired_ephemeral`) and the `expired_ephemeral_is_not_recoverable`
  security test (gone from inbox/history/search/export after expiry + sweep).

- **Idle notification dedup (WL-039).** Covered by **dual-backend store unit tests**
  (run under default sqlite **and** `--no-default-features --features libsql`):
  `supersede_prior_idle_replaces_prior_unread_idle` (two idle pings → only the latest
  unread, predecessor stamped `superseded_by` + hidden from inbox/peek/unread),
  `idle_dedup_never_touches_real_messages` (a real `send` between two pings is NOT
  superseded — the `kind='idle'` predicate is the hard real-message boundary),
  `idle_dedup_only_supersedes_unread` (a read predecessor is spared),
  `idle_dedup_scoped_to_same_sender_recipient`, `idle_dedup_authz_self_only` (the
  sender-scoped guard — peer B cannot dedup peer A's pings), and
  `idle_dedup_idempotency_replay_is_noop` (the `id <> new_id` guard makes an
  idempotency-key replay a clean no-op, never self-supersede), plus the libSQL twins
  which also exercise the **positional `kind` projection at index 12**. The CLI seam is
  driven through the compiled binary in `tests/integration.rs`
  (`cli_notify_dedup_idle_collapses_to_latest_and_spares_real_message` and the
  `..._without_dedup_idle_keeps_both_unread` negative), the MCP seam by an `McpServer`
  test (`mcp_weave_notify_dedup_idle_collapses_and_spares_real_send`), and the
  zero-standing-token contract by `catalog_weave_notify_lists_dedup_idle` +
  `standing_mcp_surface_is_within_token_budget`.

- **Canonical session export/import (WL-040).** Three layers, no stubs.
  **Pure unit** (`weave-core/src/session.rs`): `serialize_session → to_json →
  from_json` round-trips messages, asks, and memory byte-for-byte; `from_json`
  **rejects** a wrong format magic and a `schema_version` newer than the build, and
  **tolerates** unknown extra fields (additive `#[serde(default)]`); `synth_idempotency_key`
  is deterministic, bounded, sanitizes a hostile identity, and varies per source id;
  empty-session round-trips. **Integration** (`tests/integration.rs`, runs under
  *both* backends since it drives the compiled binary):
  `session_export_import_round_trips_across_distinct_dbs` is the headline cross-DB
  portability proof (a message sent into DB-A appears for the identity in a *fresh*
  DB-B after export→import — proving id remap), `session_import_is_idempotent_on_reimport`
  (the same file imported twice inserts each message once),
  `session_import_dry_run_writes_nothing`, and `session_export_import_round_trips_mesh_memory`
  (a global memory entry written in instance A, with its own `XDG_CONFIG_HOME`, is
  readable in instance B with a *different* config home). **Security**
  (`tests/security.rs`): the import file is an untrusted parser + DB write path —
  an over-`MAX_BODY` body, a control-char identity, and a malformed idempotency key are
  each **rejected cleanly** (non-zero exit, no partial-write — the inbox re-opens with
  zero messages); a body of `'; DROP TABLE messages; --` + `$(…)`/backtick metachars
  imports as **literal text** (byte-identical round-trip, DB intact, no shell);
  `--out` overwrite/missing-parent and `--in` missing/directory/non-weave-JSON are
  guarded. **No new `Store` method, no schema change, no new standing MCP tool** —
  so the libSQL CI job and the standing-budget test are known-unaffected.

### Tracked ask/answer/ack (P1)

The ask/answer/ack lifecycle is tested **hermetically across both backends** and
end-to-end. The pure monotonic state machine is locked by Property 5 above
(`AskState::can_transition`). The store layer (`src/store.rs` /
`src/store_libsql.rs` `#[cfg(test)]`, run under default sqlite **and**
`--no-default-features --features libsql`) covers a full `ask → answer → ack`
roundtrip: `ask` opens an `open` row + mints a valid `ask_id_valid` correlation_id
and inserts the question into `messages`; `answer` addresses back to the asker, sets
`answer_msg_id`, transitions `open → answered`; `ack` transitions `→ acked`, stamps
`closed_ts`, and records the optional `close_note`; `reply_to` chaining acks the prior
thread and links the new question into the same conversation. The **failure paths** are
asserted as clean errors (never a panic): answering an acked thread, a double-ack, an
unknown correlation_id, a wrong-owner write, a broadcast `askee`, and an oversized body /
invalid correlation_id (rejected before any bind). The `McpServer` + `CARGO_BIN_EXE_weave`
black-box layers assert the **honest delivery verdict** vocabulary
(`transport_delivered` / `queued_next_turn` / `recipient_not_injectable`) appears and that
a queued / not-injectable ask is **not** an `isError` (hermetic ⇒ no real mux), plus the
`weave asks` / `weave ask-get` `--json` shapes.

### Ask-many fan-out + read-time aggregate (P2)

`ask_many` / `ask_many_result` are tested **hermetically across both backends** the same
way. The pure aggregate classifier `model::classify_ask_many` is locked by a proptest
(`classify_ask_many_is_total`): for any mix of child counts the **totality**
`answered + acked + pending + failed == target_count` holds and `state == Complete` iff
`pending == 0` (and `Partial` only under a positive elapsed `age` threshold — there is no
ticker). The store layer (sqlite **and** `--features libsql` `#[cfg(test)]`) asserts the
**parent↔child** model: `create_ask_many` inserts one `ask_groups` parent + one well-formed
P1 child ask per de-duped peer; a child answered/acked through the **unchanged** P1
`answer`/`ack` updates the read-time aggregate; the rollup tracks mixed child states with
totality preserved. **Best-effort per child** is asserted directly — an invalid/broadcast
peer in the list records a per-child error and is skipped (counted `failed = target_count -
created`) while the call still succeeds, whereas an empty or over-`MAX_ASK_MANY_TARGETS`
(64) list is a hard whole-call error. The **legacy `parent_id` migration** is locked in both
backends (`legacy_asks_gains_parent_id_and_ask_groups` + the `_libsql` mirror): a DB whose
`asks` predates ask-many is seeded with an old-schema row, opened, and the old ask reads back
with `parent_id == None` while a fresh fan-out works and re-opening is a no-op — the additive
guarded-column template. The libSQL write-trap is asserted as the first statement of
`create_ask_many` (`ask_many_write_traps_on_readonly_libsql`). The `McpServer` +
`CARGO_BIN_EXE_weave` black-box layers cover the happy fan-out (parent_id + per-child cids +
verdicts), the best-effort 1-created-1-failed path, the `complete | partial | pending`
result rendering, and the failure paths (empty / over-cap / unknown / invalid parent →
`isError`); `tests/security.rs` enforces the N-cap from the binary and that the result is
bounded (child rows ≤ `target_count`) and secret-free.

### Poll-only job board (P3)

The durable job board is tested **hermetically across both backends** and end-to-end.
The pure `model::JobState::can_transition` machine is locked by proptests
(`job_state_machine_totality`, `job_lifecycle_terminal_is_absorbing`): for all
`(from, to)` pairs the check never panics and is deterministic, **no edge leaves a
terminal state** (`{Completed, Failed, Cancelled, Expired, Unavailable}` is absorbing,
idempotent self-noop excepted), and cancel/expire are reachable from every non-terminal
state; a companion property asserts the **`attempt_id` fencing uniqueness** — across a
sequence of claims, only the latest minted token validates and every prior one is
rejected. The store layer (`src/store.rs` / `src/store_libsql.rs` `#[cfg(test)]`, run
under default sqlite **and** `--no-default-features --features libsql`) covers the full
`create → claim → update → complete → result` lifecycle: `create_job` opens a `queued`
row + a valid `job_id_valid` id with owner defaulting to creator; `claim_job` mints the
`attempt_id`, assigns, and moves to `running`; `update_job` with the matching token
advances state, appends the progress event, stamps `completed_ts` on terminal entry, and
stores the result. The **`attempt_id` fencing is asserted at the store level**
(`job_update_stale_attempt_is_fenced` / `job_stale_attempt_is_fenced_libsql`): a re-claim
mints a new token and an update carrying the **stale** token is rejected
(`stale_attempt`), while an unclaimed (NULL-token) job accepts a tokenless update.
**Illegal transitions** (e.g. `completed → running`) and **cooperative cancel** (queued →
terminal `cancelled`; in-flight → `cancel_requested` flag only, never a hard delete) are
asserted as clean errors / flag-sets, never panics. **Legacy-migration idempotency** is
locked in both backends (`job_migration_is_idempotent` + the libSQL re-open path): a DB
with no `jobs` table gains it on `migrate`, a second run is a no-op, and a fresh DB already
has it; `row_to_job` hard-errors on an unknown stored state. The libSQL write-trap is
asserted as the first statement of every job WRITE. The `McpServer` +
`CARGO_BIN_EXE_weave` black-box layers cover the happy `create/list/status/result` JSON
shapes and the full CLI roundtrip (capture id → claim → capture `attempt_id` → update →
complete → result), plus the failure paths (stale attempt → fenced error, unknown job →
not_found, illegal transition, oversized title/JSON → cap error, bad assignee/job id →
validator error); `tests/security.rs` enforces the text/JSON byte caps, the id validators
(a metachar id never reaches a bind), the `clamp_limit`-bounded list, and secret-free
output.

### Circles + orchestrator role (P4)

Circles and roles are tested **across both backends** and end-to-end. The pure
`model` layer locks the validators: `peer_role_round_trips_and_unknown_is_err`
(every `PeerRole` round-trips `from_str(as_str())`, an empty legacy value coalesces
to `Peer`, any other unknown is a hard `Err`), `circle_valid_accepts_good_rejects_bad`
/ `circle_or_default_maps_empty_to_default`, and a **proptest** `circle_valid_is_total`
(never panics on arbitrary input, the verdict matches the contract, a metachar is
always rejected — the `ask_id_valid`/`job_id_valid` totality precedent). The config
layer asserts `Config::circle()` resolves `"default"` when unset/blank, passes a
valid value through, and **sanitizes** a metachar/oversized value back to `"default"`
(`circle_resolves_default_passthrough_and_sanitize`). The store layer
(`src/store.rs` / `src/store_libsql.rs` `#[cfg(test)]`, run under sqlite **and**
`--features libsql`) covers: **circle/role migration idempotency on a legacy peers
table** (`legacy_db_without_circle_role_migrates_in_place` in both backends — a
pre-P4 `peers` gains the two columns, a legacy row reads `circle='default'`/
`role='peer'`, re-open is a no-op); `register_roundtrips_circle_and_preserves_role`
(register round-trips the circle and a re-register does **not** demote an
orchestrator — the role-preserving upsert); `claim_refuses_live_holder_then_force_steals`
(a non-force claim while a LIVE holder exists ⇒ `Refused` with no write; `force` ⇒
`Claimed` and the prior holder demoted to `peer`; an unregistered caller ⇒ `Err`);
`list_peers_in_circle_scopes` (`None`/`'*'` ⇒ all); and `orchestrator_status`
**liveness reuse of `is_alive`** (a fresh holder ⇒ present; a holder backdated past
the TTL window ⇒ absent — no new probe). The `McpServer` + `CARGO_BIN_EXE_weave`
black-box layers cover the happy claim/status/whoami path **and the failure paths**
(claim-without-force-when-a-live-holder-exists ⇒ a refusal *result* string, not a
protocol error; `orchestrator_status` of an empty circle ⇒ "no live orchestrator";
`weave_whoami` echoes circle + role), the CLI circle scoping (`--circle`/
`--all-circles`), and the **load-bearing backward-compat regression**
(`cli_peers_default_circle_human_output_unchanged`: with everyone in `"default"`
and no flag the human `peers` line carries no `circle=`/`role=` token — byte-identical
to pre-P4). `tests/security.rs` enforces the circle caps and enum discipline:
`invalid_weave_circle_is_sanitized_to_default` (a metachar/oversized/control
`WEAVE_CIRCLE` is sanitized, never stored raw, never crashes),
`role_is_never_free_text` (the only path to `orchestrator` is `claim`; a fresh
register is always `peer`), and `orchestrator_output_is_secret_free`. **Liveness
fixture note:** the orchestrator-status integration tests register under a forced
foreign `HOSTNAME` and query without it, so a row's stored host differs from the
query-time `this_host` ⇒ liveness fails OPEN (TTL recency-online) rather than
pid-probing a one-shot CLI process's already-dead PID — the same remote-host fixture
the scan/peers liveness tests use.

### Rich presence: turn_state + description (P5)

Rich presence is tested **hermetically across both backends** and end-to-end. The
pure `model` layer locks the enum and the TTL: `TurnState` round-trips
`from_str(as_str())` for every variant (empty/`unknown` ⇒ `Unknown`, any other value
⇒ a hard `Err`), and a **proptest** asserts `expire_description` **totality** (never
panics for any `(now, description_ts)` incl. `i64::MIN`/`MAX`/negatives via
`saturating_sub`; blanks iff non-empty, anchored, and `now - ts >= DESCRIPTION_TTL_SECS`;
never mutates a fresh-within-window description). The store layer (`store.rs` /
`store_libsql.rs` `#[cfg(test)]`, run under sqlite **and** `--features libsql`) covers:
**migration idempotency on a legacy peers table** (a pre-P5 `peers` gains the three
columns; a legacy row reads `turn_state=''`(unknown)/`description=''`/`description_ts=0`;
re-open is a no-op); `set_turn_state` updates **only the named row** and rejects a
non-enum value (`Err`, no write); `set_description` round-trips, sanitizes (control-strip
+ 200-char cap, oversized truncates rather than erroring), and stamps `description_ts=0`
on clear; the **read-time TTL** (poke `description_ts` past `DESCRIPTION_TTL_SECS` via a
direct UPDATE ⇒ `get_peer`/`list_peers` read `description==""`, while a fresh one within
the window is honored and the **stored row is never mutated**); and the
register-preserving upsert (a re-register does **not** clobber a self-set
turn_state/description — the `role`-omission precedent). The `McpServer` +
`CARGO_BIN_EXE_weave` black-box layers cover the happy `set_turn_state`/`set_description`/
`whoami` path **and the failure paths** (a bad turn_state ⇒ an `isError` result, not a
protocol crash; an oversized description truncates, never errors), the **hook
auto-transitions** (`hook session` ⇒ `pending_first_turn`, `hook prompt` ⇒ `working`,
`hook stop` ⇒ `idle`, `hook notification` ⇒ `awaiting_input`, each surfaced in `weave
peers`) including that a turn_state-write failure **never sinks the drain**, and the
**load-bearing backward-compat regression** (an unset peer's `peers`/`sessions`/`scan`
human line is byte-identical to pre-P5; `--json` only ADDS keys). `tests/security.rs`
enforces the caps and the **owner-only** rule: a description is control-stripped
(newline/NUL/ESC) and capped, a non-enum turn_state is rejected, surfaced fields are
secret-free, and a caller **cannot set another peer's** turn_state/description (every
setter is an UPDATE bound to the caller's own resolved identity).

### notify_peer + delivery observability (P6)

P6 is tested **hermetically across both backends** and end-to-end. The pure `model`
layer locks the trace vocabulary: `DeliveryRefKind`/`DeliveryStage`/`DeliveryOutcome`
each round-trip `from_str(as_str())` for **every** variant (unknown ⇒ a hard `Err`) —
the exhaustiveness lock for the enum-as-TEXT columns. The store layer (`store.rs` /
`store_libsql.rs` `#[cfg(test)]`, run under sqlite **and** `--features libsql`) covers:
**migration idempotency on a legacy DB** (a pre-P6 DB with no `delivery_log` gains it
on open; `record`/`list` work; re-open is a no-op); `record_delivery` append →
`list_delivery` returns stages **oldest-first** (`ts ASC, id ASC`) and the body **never**
appears in a trace row (secret-free at the store seam); the read is **bounded** by
`MAX_DELIVERY_ROWS` regardless of the requested limit; `gc` prunes old `delivery_log`
rows in the **same retention pass** while keeping recent ones; and (libsql)
`record_delivery` **traps on a read-only handle first** (owner-only-writes). The pure
`verdict_to_stage` fold has a `mcp.rs` unit test pinning each verdict token → (stage,
outcome) and the safe `Queued/Ok` default. The `McpServer` + `CARGO_BIN_EXE_weave`
black-box layers cover the happy `weave_notify` path (verdict token, NOT `isError`,
even for an unknown peer — degrade-to-store) **and the failure paths** (broadcast `to`
⇒ `isError` pointing to send; oversized body ⇒ `isError`, no partial persist;
`weave_delivery` of an unknown ref ⇒ the empty-trace line, not an error), the **drain
trace** (a `prompt` mark-read drain records a `drained` stage; a `stop` peek does not),
and the **send/receipts regression-lock** (send + receipts output and read-marking are
byte-identical with the trace present — the trace is purely additive). `tests/security.rs`
asserts the **secret-free** invariant directly (a hostile body marker never appears in
any `delivery` row — human, `--json`, or MCP), the caps (oversized notify body rejected),
and **no-shell** handling of a metachar `to` (bound as a literal, no shell sentinel; a
control-char `to` is rejected). The trace is **best-effort** everywhere — a write failure
logs to stderr and never sinks delivery (the `set_turn_state_best_effort`/gc precedent),
and there is **no `store → inject` edge** (the store records the outcome it is passed
after the inject; neither backend imports `inject`).

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

### Hermetic presence-dashboard (`weave sessions --watch`) testing

A re-rendering watch loop is normally the hardest thing to test (it sleeps and
never returns). weave keeps it fully hermetic by splitting the loop into a **pure
render** and a **bounded** driver — every timing-bounded test is bounded by
**iteration count, never a wall-clock assertion**:

- **Pure render unit tests (in `src/main.rs` `#[cfg(test)]`):**
  `render_sessions_dashboard(rows, opts, this_host, now)` is a pure function, so
  the frame is asserted from hand-built `SessionRow`s against a **fixed `now` + a
  fixed `this_host`** (never `model::now()` / the real hostname / the wall clock),
  making output byte-deterministic — the dashboard classifies each row's liveness
  itself via `liveness_from_fields`, so the host-aware verdict is fully determined
  by the two pinned inputs. Cases cover grouping by `(repo, branch)`, the header
  three-count breakdown (`N local-alive, M remote-alive, K stale` + #repos /
  #branches), per-group alive/total, the `--repo`/`--branch` filter echo, `+N more`
  truncation past the row budget, the empty-snapshot `no sessions` body, the
  empty-tag `-` rendering, and ANSI-on vs. plain (the only byte difference is the
  `\x1b[2J\x1b[H` clear-home prefix). A dedicated mixed-liveness case forces
  `this_host` to a non-matching host so a row whose `host` differs reads
  `alive (remote, ttl)` + the ` <remote>` marker deterministically (a same-host
  pid-bearing row asserts the membership set `pid|stale` since it probes the live
  test pid — never an exact same-host verdict), and asserts the three-count header.
- **Bounded integration path (`tests/integration.rs`, via `CARGO_BIN_EXE_weave` +
  scrubbed env + temp `WEAVE_DB`):** `weave sessions --watch --iterations 1` (and
  `--iterations N` for multi-frame) renders exactly N frames and **exits 0** — the
  harness `run_ok` *returning at all* proves there is no hang, with **no `sleep`
  to "wait for" a frame** and no elapsed-time assertion. The frame is asserted to
  carry both peer names, a group header, the per-row `[<reason>]` marker, and the
  `N local-alive, M remote-alive, K stale` three-count header; a remote-host row
  (and its ` <remote>` marker + `[alive (remote, ttl)]` reason) is driven by the
  same forced-`HOSTNAME` + `WEAVE_PEER_DBS` foreign-store fixture the `scan`
  remote-surfacing test uses, so the remote verdict needs no wall-clock backdate.
  `--watch --repo`/`--branch` narrows it; `--watch --json` emits a single snapshot
  with **no clear prefix**.
- **Read-only proof:** capture the `WEAVE_DB` file bytes before and after
  `weave sessions --watch --iterations 3` with **no explicit identity** (so even
  the one pre-loop self-refresh is skipped) and assert the store is byte-unchanged
  across ticks — proving the loop writes nothing.
- **Escape-free capture:** integration runs set `WEAVE_NO_CLEAR` (and/or run
  non-TTY) so captured stdout is plain text with no ANSI clear-home to match
  around — the clear prefix is asserted only by the pure unit test that toggles
  `opts.clear` directly.
- **Clamp proptest (`tests/prop.rs`):** `clamp_watch_interval` totality — any
  `u64` maps into `[1, 3600]` and the clamp is idempotent (mirrors the
  `parse_clamp_timeout` property).

Both backends run this suite: the render consumes store types compiled under
`--features libsql` too, and since there is **no new column** there is no
migration/roundtrip case to add (see §6).

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
fail-open / Turso shared-DB case), the `liveness_for` host-aware matrix against
**real libSQL rows** (the same fixed-`this_host` + fixed-`now_ts` regimes, with the
stale paths seeded via the `backdate_peer` helper, plus the `is_alive` delegation
regression-lock), the `open_readonly` read-only proof (a write
through the RO handle is engine-rejected and the foreign DB file stays
byte-identical), and — for the #7 multi-key registry —
`keys_register_get_list_roundtrip_libsql` (append + remove semantics),
`register_key_enforces_per_identity_cap_libsql` (the `MAX_KEYS_PER_IDENT` cap on the
alt backend), and `legacy_single_key_migrates_into_identity_keys_libsql` (the
additive, idempotent legacy-`keys`→`identity_keys` migration roundtrip under libSQL).
So both the sqlite count and the libSQL count grow together when
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

The same integration cases ALSO prove the per-source token covers `peer_dbs`
(`WEAVE_PEER_DBS`) federation remotes (item-2 confirmation): a `WEAVE_PEER_DBS="LABEL=libsql://…"` remote with its
own `WEAVE_PULL_TOKEN_<LABEL>` resolves the `PerSourceLabel` token tier in `weave
doctor --json` (proving the LABEL namespace reaches `peer_dbs`, since
`peer_db_sources` and `pull_from_sources` share one resolver), with neither token byte
printed. No second peer_db token resolver exists or is tested.

#### Per-source remote-call timeout (`WEAVE_PULL_TIMEOUT_MS[_<LABEL>]`)

The per-source timeout is tested as **resolution + clamp + doctor observability**,
hermetically — never a real network bound:

- **Unit (`config`):** `parse_clamp_timeout` (`"200"`→`Some(200)`; `"0"`/`"abc"`/`""`/
  negative → `None`; `"10"`→`Some(50)` clamp-UP; `"99999999"`→`Some(600000)` clamp-DOWN;
  the exact bounds pass through) plus a **proptest** that it is total on arbitrary input
  and any `Some(n)` is within `[MIN_TIMEOUT_MS, MAX_TIMEOUT_MS]`. `per_source_timeout`
  precedence (per-source label-env wins; set-but-garbage falls through to the global;
  global-only → `Global`; neither → `(None, Default)`), serialized via the canonical
  `crate::testenv::lock_env()` guard (§1).
  `resolve_store_sources_with_tiers` carries the clamped `timeout_ms` onto
  `StoreSource::Remote` and returns the `PerSourceLabel` / `Global` timeout tier, and
  the default-tier doctor method substitutes `REMOTE_TIMEOUT_MS_DEFAULT`.
- **Integration (scrubbed env, both backends):** a labelled remote with
  `WEAVE_PULL_TIMEOUT_MS_<LABEL>` + a global `WEAVE_PULL_TIMEOUT_MS` ⇒ `weave doctor
  --json` reports `federation_remote_timeout_{per_source,global,default}` counts and a
  `federation_remote_timeout_ms_{min,max}` range within `[50, 600000]`; the human
  doctor and the MCP `weave_doctor` carry a `remote timeout:` line. The counts hold on
  BOTH backends (resolution is backend-agnostic); the libsql-only skip note is gated by
  `cfg!(feature="libsql")`. No token byte appears in any timeout output (the security
  redaction test configures a token + a per-source timeout together).

#### `weave doctor` federation-health rollup (both source kinds)

The federation-health rollup that surfaces the `pull_from` token/timeout tiers at
parity with `peer_db` is tested **hermetically** — env-guard-serialized, `.invalid`
hosts only, **no live network**:

- **Unit (`config`):** `pull_from_remote_token_tiers` resolves per-source / shared /
  none over `pull_from`, locals omitted, and yields the identical tier multiset as the
  same list under `peer_dbs` (proving the symmetric accessor shares one resolver); its
  `Debug` carries no token. `federation_health()` aggregation is asserted over a mixed
  `.invalid` set (a labelled remote with a per-source token + per-source timeout, an
  unlabelled remote on the shared token + global timeout, and a local) — symmetric
  `peer_db == pull_from` rollup, all `total`/`local`/`remote` counts and token/timeout
  tiers, and the `ms_min`/`ms_max` range; plus the empty / local-only edge (defaulted
  zeros, `ms_min`/`ms_max == None`, no misleading `0-0`) and the no-token/no-timeout
  default tier (`token_none`, `timeout_default`, ms == `REMOTE_TIMEOUT_MS_DEFAULT`).
- **Integration (scrubbed env, both backends):** with a mixed `WEAVE_PULL_FROM` (one
  local + one labelled `.invalid` remote carrying `WEAVE_PULL_TOKEN_<LABEL>` +
  `WEAVE_PULL_TIMEOUT_MS_<LABEL>`) alongside `WEAVE_PEER_DBS`, `weave doctor --json`
  carries the additive `federation_pull_*` keys (`federation_pull_sources`,
  `_local`/`_remote`, `_token_{per_source,shared,none}`,
  `_timeout_{per_source,global,default}`, `_timeout_ms_{min,max}`) with correct
  counts/tiers, and every existing `federation_*` key is unchanged. A local-only case
  asserts the pull-side block (and its keys) is **absent** (additive-when-configured).
  Resolution is backend-agnostic, so the tiers hold on the default sqlite build (remote
  loud-rejected at the store seam) and the libsql build.
- **Security / redaction:** with a per-source token configured, **no** token substring
  appears anywhere across the new pull-side surface — `doctor --json`, the human
  `doctor` block, OR the `weave_doctor` MCP result (tier counts only).

These run under BOTH the default sqlite and the `--features libsql` backends.

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
   - **Reads or writes a `WEAVE_*` (or any process-global) env var → serialize on
     `crate::testenv`** (§1): `let _g = crate::testenv::lock_env();` first, then
     mutate via `EnvVarGuard` so the value restores even on panic. Never call
     `set_var`/`remove_var` on a `WEAVE_*` var in a unit test without the lock —
     the multithreaded runner will otherwise race. (Integration/security/prop
     tests are exempt — separate process, scrubbed env.)
2. **New CLI subcommand / flag → an integration test** in `tests/integration.rs`
   using the `tests/common` helpers (`run_ok`, `run`, `run_hook`,
   `run_stdin_full`). If it has machine-readable output, assert the `--json`
   shape, not just substrings. Every new top-level command must also be added to
   the `weave tui --json --pane commands` ledger with `mcp_decision`,
   `status_surface`, `help_smoke`, `behavior_coverage`, `docs_surface`,
   `tui_exposure`, and `risk`; `tui_once_and_json_are_default_build_operator_surfaces`
   fails if any classification is missing.
3. **New MCP tool / protocol behavior → an `McpServer` test** (`spawn`,
   `call_tool`, assert `isError` and the returned text). Include the failure path
   (bad/oversized args → `isError`, never a panic or silent persist).
4. **New injector backend or shaping rule → unit tests** in `src/inject.rs`
   asserting the exact argv table, the end-of-options `--` guard for
   leading-dash bodies, the empty/whitespace no-op, and `id_valid` rejection of
   malicious target ids. If a real spawn matters, extend the fake-mux harness.
5. **A new invariant ("for any input, X holds") → a proptest property** in
   `tests/prop.rs`. Keep `cases` small (subprocess-heavy) and
   `failure_persistence: None`. **Crypto/`sign` tests must be deterministic**: seed
   every key from **fixed bytes** (`SigningKey::from_bytes(&[seed; 32])` / a
   `test_key(seed)` helper), never `OsRng` — ed25519 verify is RNG-free, so a
   correct test is bit-stable. Repeat-run any new crypto proptest (e.g.
   `for i in $(seq 1 20); do cargo test --features sign <name> || break; done`) to
   prove no flake before handoff.
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
    `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, the
    libSQL clippy/build/**test** column, and — when the crypto path is touched —
    the `sign` and `libsql sign` columns
    (`cargo clippy --all-targets --features sign -- -D warnings` /
    `cargo test --features sign`, and the same with
    `--no-default-features --features "libsql sign"`).

    CI mirrors this exactly. The GitHub Actions workflow (`.github/workflows/ci.yml`)
    runs six jobs — `rustfmt`, `clippy`, `test` (default sqlite), `build (libsql
    backend)` (now clippy **+ build + test** for the libSQL backend), `sign` (sqlite
    + `sign`: clippy + test), and `libsql + sign` (clippy + build + test). The
    optional crypto path is therefore gated in CI on **both** backends, not just
    locally, so a `sign`-only regression cannot merge unnoticed.

### Supply-chain advisory gate (WL-075)

CI's `audit` job uses `cargo-deny check advisories` plus `deny.toml`'s explicit,
scoped ignores for the optional libsql remote-TLS advisory surface. Reproduce the
same posture locally with:

```bash
python3 scripts/supply_chain_audit.py
```

The helper checks three repo-local invariants before running cargo-deny:

- `deny.toml` still has `[graph] all-features = true` and exactly the tracked
  advisory ids (the removed bincode id must not reappear).
- The default sqlite dependency graph remains advisory-clean for
  `rustls-webpki` (`cargo tree -i rustls-webpki --locked` must find nothing).
- The residual `rustls-webpki` tree is still confined to the optional libsql TLS
  graph (`--no-default-features --features libsql`).

If `cargo-deny` is not installed, install it with
`cargo install cargo-deny --locked`. For environments that cannot install tools,
`python3 scripts/supply_chain_audit.py --allow-missing-cargo-deny` still verifies
the local policy/tree invariants and reports the missing deny binary as a warning.
`python3 scripts/supply_chain_audit.py --self-test` is stdlib-only and pins the
script parser/formatter behavior.

## Generated target-output smoke matrix (WL-081)

Unit and integration tests prove source behavior, but operators also need proof
that the **generated binaries in `target/`** were produced by Cargo and work when
executed directly. Use the stdlib-only smoke runner:

```bash
python3 scripts/target_smoke.py
```

What it proves:

- `cargo metadata.target_directory`, `target/CACHEDIR.TAG`, `.rustc_info.json`,
  debug/release directories, and binary metadata are recorded in a JSON report.
- `target/debug/weave` and `target/release/weave` are executed directly, not via
  `cargo test`.
- Each artifact runs a temp-store E2E sweep: `--version`, `doctor --json`,
  registration, `send`/`inbox`, `delivery`, `ask`/`responder`/`answer`, jobs,
  graph, session export/import dry-run, backup, MCP stdio meta-tool, readonly-DB
  negative, unknown-peer negative, and CC Switch missing-DB diagnostic when the
  sqlite-only provider-switch command is compiled in.
- The report is machine-readable at `target/target-smoke/target-smoke.json` by
  default and the generated report stays out of git with the rest of `target/`.

For the larger feature-gated artifact sweep, run:

```bash
python3 scripts/target_smoke.py --full
```

`--full` builds feature-specific artifacts in isolated target dirs under
`target/target-smoke/build/` so sqlite/libsql/sign/surfaces/obscura combinations
cannot overwrite the root debug/release artifacts. The script records skipped
feature artifacts when run without `--full`.

To prove Cargo recreates the generated cache from a no-`target` state:

```bash
python3 scripts/target_smoke.py --clean-target
```

`--clean-target` refuses to delete a non-empty `target/` unless Cargo's
`CACHEDIR.TAG` marker is present. Never commit files from `target/`; commit only
script, docs, or source changes.

For operator machines that use rustup, the smoke runner can also enforce the
toolchain-cache hygiene expected after a refresh/prune pass:

```bash
python3 scripts/target_smoke.py --check-rustup-hygiene
```

This fails when stale date-pinned nightlies or version-pinned stable duplicates
remain beside the current `stable-*` and `nightly-*` aliases. The pure parser
coverage for that check is available without building artifacts:

```bash
python3 scripts/target_smoke.py --self-test
```

### Dimensional doctor/scan liveness diagnostics (WL-068)

`peers --json`, `scan --json`, and `doctor --json` now expose orthogonal
dimensions instead of relying only on a folded status token: `registered`,
`process_expected`, `process_alive`, `pane_alive`, `injectable`, `reachable`,
`responsive_recently`, `last_heartbeat`, `last_transport_success`,
`last_response`, `stale_reason`, and `inject_probe`. The integration suite pins
misregistration, responsive-answer, and registered-stale cases so a peer can be
accurately described as, for example, "registered but process-dead" or
"reachable but heartbeat-stale" without doctor/scan hiding the dimensions. MCP
`weave_peers`, `weave_scan`, and `weave_doctor` mirror the same dimension summary
in their text surfaces.

### Unsafe shared-target delivery avoidance (WL-069)

Diagnostics alone are not enough for shared mux targets. When more than one peer
shares the same `(mux, target, socket)` tuple, point-to-point live injection now
degrades to queue-only and records a delivery trace row with stage
`not_injectable` and outcome `ambiguous_target`. The CLI/MCP verdict token is
`ambiguous_target_queued`. Integration coverage registers two peers on the same
fake tmux pane, sends a notification, and proves no `injected` or
`inject_failed` trace row is produced.
