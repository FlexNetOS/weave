# weave

**Rust-native agent-to-agent session mesh with a native injector.**

Let coding-agent sessions (Claude Code, etc.) message each other — and **push into a
running session's terminal pane** (tmux *or* zellij) so a peer is flagged the moment a
message arrives. One static binary. No Python, no daemon, no external dependency on
repowire.

See [PRD.md](PRD.md) for the full design and [TASKS.md](TASKS.md) for the roadmap.

## Why

Claude Code sessions are isolated. Prior local tools were either **poll-only** (no push)
or **tmux-only + Python** (no native zellij injector). weave is a single Rust binary that
pushes natively into tmux and zellij, and degrades to hook-delivery-on-next-turn when no
multiplexer is present.

## Build

```bash
cargo build --release      # -> target/release/weave
```

## Use with Claude Code

Register the MCP server (per user, all projects):

```bash
claude mcp add weave --scope user -- /path/to/weave mcp
```

Wire lifecycle hooks in `~/.claude/settings.json` so sessions auto-register and
auto-receive (use `weave setup` to do all of this automatically):

```jsonc
{
  "hooks": {
    "SessionStart":      [{ "hooks": [{ "type": "command", "command": "weave hook session" }] }],
    "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": "weave hook prompt" }] }],
    "Stop":              [{ "hooks": [{ "type": "command", "command": "weave hook stop" }] }]
  }
}
```

Now any session can use the `weave_*` MCP tools, and `weave hook prompt` surfaces unread
messages into the agent's context on its next turn (auto-delivery without a multiplexer).

## CLI

```bash
weave register --name desktop        # register this session (captures pane from $TMUX_PANE/$ZELLIJ_SESSION_NAME)
weave attach --name desktop          # adopt a *running* session into the store WITHOUT restarting (re-capture pane)
weave peers                          # list peers + whether each is alive + injectable (now with repo/branch/worktree tags)
weave scan                           # refresh your own git tags, then list every (federated) peer + liveness + tags
weave scan --repo weave --branch feat/x --json   # filter by exact repo/branch tag; machine-readable output
weave sessions --watch               # live read-only presence dashboard, grouped by repo then branch (Ctrl-C to stop)
weave sessions --watch --interval 5 --repo weave  # re-render every 5s, narrowed to one repo
weave connect --to envctl            # probe whether a peer can be live-nudged right now (verdict only)
weave send --from desktop --to envctl --body "apply the rtk fix"
weave inbox --me envctl              # read (marks read); --peek to not mark; --all to include read
weave inject --to envctl --text "live nudge"   # test the injector directly
weave mcp --session desktop          # run the MCP stdio server

# Cross-store delivery (Tier-2): deposit a message for a recipient in another store
weave send --from desktop --to envctl --to-store /path/to/other/messages.db --body "ship it"
weave outbox                         # inspect pending cross-store intents you queued (--json)
weave pull --me envctl               # pull + commit intents from your pull_from sources now
```

Identity resolution: `--from/--me/--name` > `$WEAVE_SESSION` > basename of cwd.
Send `--to all` (or `*`) to broadcast; read state is tracked per-reader.

`weave attach` captures the current pane and upserts **your own** peer row, so a
session that started outside a mux (or before `weave setup`) becomes injectable
without a restart. `weave connect --to <peer>` reports a capability verdict —
`live` (a nudge can be delivered now), `registered but not alive` (queued for the
recipient's next turn), or `not injectable` — and is **not** an error when a peer
can't be live-nudged: its messages still arrive via the store on its next drain.

`weave scan` is the "who's around, and where?" view. It first re-captures **your
own** session's git tags and presence (owner-only — it never re-registers a
foreign row), then lists every (federated) peer joined with liveness and its
**repo / branch / worktree** tags. `--repo <name>` / `--branch <name>` narrow the
set by exact tag match; `--json` emits an array of
`{name, repo, branch, worktree, mux, pane, host, alive, origin, foreign}`. The
same repo/branch/worktree tags now also appear in `weave peers`, `weave sessions`
(via a local-only display join), and the `weave doctor` `peers_tagged` count.

**Session tags** (repo, branch, worktree id) are captured **best-effort** from
the session's cwd at registration and refreshed on `scan`: the worktree id comes
from the cwd's `.git` (the `.git/worktrees/<name>` segment for a linked worktree,
the `(main)` sentinel for a main worktree), and the branch/repo from a trusted
`git` invoked as an external program (explicit argv, no shell). A non-git cwd or
any git/fs failure simply yields empty tags — it never blocks registration.

**Presence means *alive*, not "wrote recently".** A peer reads online only when
it is within the presence TTL **and** (for a peer on this host with a known PID)
its process is still running; presence fails open for remote / cross-machine
peers that can't be probed locally.

### Live presence dashboard (`weave sessions --watch`)

`weave sessions --watch` turns the session listing into a **read-only presence
dashboard** that re-renders the federated peers (the same `scan` model: peers
joined with liveness via `is_alive`) on a fixed interval. It is **read-only** —
the watch loop writes nothing per tick (at most one owner-only self-refresh of
*your own* row runs once before the loop, exactly like `scan`, only when your
identity is explicit), so observing presence never perturbs it.

| Flag | Default | Effect |
|------|---------|--------|
| `--watch` | off | render the dashboard, looping until Ctrl-C |
| `--interval <secs>` | `2` | seconds between frames, **clamped to `[1, 3600]`** |
| `--iterations <N>` | `0` | `0` ⇒ loop forever (interactive); `N` ⇒ render exactly N frames then exit (scripting / tests) |
| `--repo <R>` | — | only sessions whose repo tag equals `R` (composes with `--watch`) |
| `--branch <B>` | — | only sessions whose branch tag equals `B` (composes with `--watch`) |
| `--json` | off | with `--watch`, emit a **single** JSON snapshot (no loop, no clear-screen) and exit |

The frame is a **header summary** —
`weave sessions [<ts>] — N session(s), A alive, R repo(s), B branch(es)` (with any
active `repo=…`/`branch=…` filters echoed) — followed by one section per
`(repo, branch)` group in sorted order. Each section header reads
`[<repo> / <branch>] G session(s), GA alive` (an empty tag renders as `-`), and
each row is `  <name> [alive|offline] worktree=… mux=… host=…`, plus ` (via <store>)`
for a federated peer from another store. A group exceeding the per-section row
budget (20) prints the first 20 rows then a `  +N more` line. An empty snapshot
renders the zeroed header plus `no sessions`.

The dashboard is **dependency-light** (std-only): the loop is
`std::thread::sleep` between frames, and the in-place redraw uses a plain ANSI
clear-home (`\x1b[2J\x1b[H`) emitted **only** when stdout is a TTY
(`std::io::IsTerminal`) and neither `NO_COLOR` nor `WEAVE_NO_CLEAR` is set —
otherwise frames print as plain escape-free text. No TUI, signal, or async crate
is pulled in.

## MCP tools

`weave_send` · `weave_outbox` · `weave_inbox` · `weave_history` · `weave_sessions` · `weave_clear`
· `weave_peers` · `weave_scan` · `weave_reply` · `weave_thread` · `weave_receipts` · `weave_doctor` · `weave_whoami`
· `weave_attach` · `weave_connect`

On `weave_send`, if the recipient is a registered injectable peer, a live nudge is pushed
into its pane; otherwise the message waits and is delivered on the recipient's next turn.

**Cross-store (Tier-2).** Pass `to_store` (a path to another store) to `weave_send` to queue
the message as a directed intent in **your own** outbox — the recipient pulls and commits it
into its inbox on its next drain (no foreign write; broadcast is refused with a cross-store
target). `weave_outbox` is a read-only self-inspection of intents you have queued that have
not yet been pulled. On the receiving side, a `weave_inbox` drain with `WEAVE_PULL_FROM`
configured pulls and commits eligible intents in the same call.

`weave_attach` adopts the calling session into the store without a restart (re-captures the
current pane and upserts the caller's own peer row only). `weave_connect` reports the same
live / registered-but-not-alive / not-injectable verdict as the CLI; only a non-existent
peer is an error, so a queued delivery is reported with `isError:false`.

`weave_scan` mirrors `weave scan`: it refreshes the **caller's own** row tags
(owner-only-writes — never a foreign row), then returns the federated peer listing
with liveness and repo/branch/worktree tags as text. Optional `repo` / `branch`
filters narrow the set by exact tag match and are bounded, so an oversized or
hostile filter argument is non-fatal (`isError:false`, never a panic).

## Native injector

| Mux | Detect (env) | Inject |
|-----|--------------|--------|
| tmux | `TMUX_PANE` | `tmux send-keys -t <pane> -l <text>` + `Enter` |
| zellij | `ZELLIJ_SESSION_NAME` | `zellij --session <name> action write-chars <text>` + `write 13` |

`commands_for()` is a pure, unit-tested function; `inject()` checks the mux is on PATH and
falls back cleanly (caller uses next-turn delivery) if the pane/session is gone.

## Storage

SQLite (rusqlite, bundled) at `~/.local/share/weave/messages.db` (override with `WEAVE_DB`),
behind a backend-agnostic `Store` trait. A **libSQL/Turso backend** is also implemented
(`--no-default-features --features libsql`) for cross-machine sync — async client driven from the
sync API via an embedded tokio runtime; local-file or remote (`libsql_url` + auth token). The
backends are mutually exclusive (each bundles SQLite); the default build uses sqlite.

### Read-only federation across stores

`weave peers` / `weave sessions` can aggregate peers and sessions from **other
projects' stores** read-only. Point `WEAVE_PEER_DBS` at a comma- (or path-)
separated list of extra DB files (or set `peer_dbs = [...]` in `config.toml`);
foreign rows are origin-tagged (` (via <store>)` in text, plus `origin`/`foreign`
fields in `--json`) and deduped on `(name, host)`. Foreign stores are opened with
`SQLITE_OPEN_READ_ONLY` and are **never written**; an unreadable store is skipped
(note on stderr), not fatal. Default (unset) behavior is unchanged — the listings
are byte-identical to a single-store run.

`peer_dbs` is read-only *visibility* only; it can never deliver a message into your
inbox. To accept cross-store *delivery* you grant the strictly-higher `pull_from`
trust (below).

### Cross-store delivery (Tier-2)

Agents in **different stores** can message each other without sharing one
`WEAVE_DB`. The model is **owner-only-writes**: a sender never writes the
recipient's store.

- **Send** queues a directed *intent* in the **sender's own** outbox
  (`weave send --to-store <recipient-store> --to <name>`). The recipient's store is
  not touched. Inspect pending intents with `weave outbox`.
- **Receive** by listing the sender's store as a delivery source —
  `WEAVE_PULL_FROM=/path/to/sender/messages.db` (comma- or path-separated), or
  `pull_from = [...]` in `config.toml`. On each drain (hook/`weave watch`) or an
  explicit `weave pull`, weave opens each allowed source **read-only**, commits the
  intents addressed to you into **your own** inbox (your store assigns the id and
  timestamp), and advances a per-source cursor. `pull_from` is **distinct** from
  `peer_dbs` (delivery vs. visibility) and capped at 16 sources.
- **Delivery is next-drain** (pull-latency-bound), not instant, and **idempotent**:
  a normal re-drain never duplicates. The single edge case is a crash *between*
  committing a message and advancing the cursor, which re-delivers **at most one**
  intent on the next drain (a bounded at-least-once guarantee).

#### Remote sources — cross-machine pull (`--features libsql`)

A `WEAVE_PULL_FROM` / `WEAVE_PEER_DBS` entry may be a **remote URL**
(`libsql://`, `https://`, or `wss://`) instead of a local file, so you can pull
cross-store delivery from a Turso / libSQL database on another machine. Set the
auth token with `WEAVE_PULL_TOKEN` (preferred — env over config; it is secret and
redacted in logs) or `pull_token = "..."` in `config.toml`.

- **Remote sources require a `--no-default-features --features libsql` build.** The
  default (sqlite) build does **not** support remote sources: it skips any remote
  entry with a loud stderr note and processes only local sources, so a mixed list
  still works for its local entries.
- **Owner-only-writes holds cross-machine.** weave opens a remote source read-only,
  reads only the intents addressed to you, and commits them into **your own** local
  inbox; it **never writes the remote source** (no schema, no migration, SELECT-only,
  every write trapped). Each remote call is time-bounded; a failed or timed-out
  remote is skipped, not fatal, and delivery stays bounded-once.
- **Cross-machine liveness stays TTL-only** — a remote-host peer reads online purely
  from the presence TTL; weave never probes a PID on another machine.

> **Deployment recommendation — use a read-only Turso token.** libSQL (0.9.30) has
> **no client-side read-only handle**, so the read-only scope is enforced by the
> token the server validates. Mint a server-enforced read-only token for the source
> DB and set it as `WEAVE_PULL_TOKEN`:
>
> ```bash
> turso db tokens create <db> --read-only
> ```
>
> This is defense-in-depth on top of weave's own read-only enforcement (SELECT-only
> + write-guard + commit-local). weave cannot mint or verify the token's scope; that
> guarantee is the server's.

##### Per-source tokens — distinct token per remote (`LABEL=url`)

When you pull from more than one remote, each may need its **own** auth token.
Prefix a remote entry with a short label, `LABEL=<remote-url>`, to select a
distinct token from the env var `WEAVE_PULL_TOKEN_<LABEL>`:

```bash
export WEAVE_PULL_FROM="PROD=libsql://prod.turso.io,STAGE=libsql://stage.turso.io,libsql://shared.turso.io"
export WEAVE_PULL_TOKEN_PROD="…"     # token for the PROD source
export WEAVE_PULL_TOKEN_STAGE="…"    # token for the STAGE source
export WEAVE_PULL_TOKEN="…"          # shared fallback (used by the unlabelled source)
```

- The **LABEL is not a secret** — it only names which env var holds the token, so
  inlining it in the source list is safe. The **token is** secret, so it stays in
  the env var: never inline a token. A label is uppercased (`prod=` and `PROD=`
  both look up `WEAVE_PULL_TOKEN_PROD`), charset `[A-Za-z0-9_]`, max 64 chars.
- **Token precedence per remote source:** per-source `WEAVE_PULL_TOKEN_<LABEL>` →
  shared `WEAVE_PULL_TOKEN` / `pull_token` → none. A per-source token is sanitized
  (same length cap + control-char reject as the shared token); if it fails that
  check it **falls through** to the shared token rather than suppressing it.
- A label only applies to a **remote URL**. An entry with no label, an invalid
  label, or a non-remote right side (e.g. a local path that happens to contain `=`)
  behaves exactly as before and uses the shared token — fully backward compatible.
- `weave doctor` reports token-free aggregate counts of how the remote sources
  resolved their token (per-source / shared / none) on a `remote tokens:` line; it
  never prints any token bytes.

> Per-source tokens only choose **which** token is sent to each source. They grant
> no new network or write capability — remote sources stay read-only and
> owner-only-writes is unchanged.

The per-source LABEL namespace (and the `LABEL=` prefix) applies to remotes in
**both** `pull_from` **and** `peer_dbs` — they share one resolver — so a labelled
remote authenticates with its own `WEAVE_PULL_TOKEN_<LABEL>` whichever list it
appears in.

#### Per-source remote-call timeout

Each remote source's connect + read timeout (ms) is also resolvable per source, on
the **same** LABEL namespace as the token, via `WEAVE_PULL_TIMEOUT_MS_<LABEL>`:

```bash
export WEAVE_PULL_TIMEOUT_MS_PROD=250   # PROD's remote calls bounded at 250 ms
export WEAVE_PULL_TIMEOUT_MS=1000       # global fallback for the rest
```

- **Timeout precedence per remote source:** per-source `WEAVE_PULL_TIMEOUT_MS_<LABEL>`
  → global `WEAVE_PULL_TIMEOUT_MS` → default `5000` ms.
- The value is parsed as a positive integer and **clamped to `[50, 600000]` ms**. A
  `0` / unparsable / out-of-range value **falls through** to the next tier — the
  bound is **never disabled** (an unbounded remote could hang a drain).
- The timeout is **not a secret**; it bounds only reads (remotes stay read-only).
- `weave doctor` reports a token-free `remote timeout:` line — per-source / global /
  default tier counts plus the effective ms range over the configured remotes — and
  the JSON keys `federation_remote_timeout_{per_source,global,default}` /
  `federation_remote_timeout_ms_{min,max}`.

#### Live nudge on a pulled message — DEFAULT ON (consent)

When a pull commits a message from an allow-listed source, weave **also fires a
content-free, paste-safe nudge** ("check your inbox") into **your own** pane by
default. The message body is never in the keystroke, only ever your own pane is
touched (never a foreign pane), and the sender can never inject.

**Residual risk:** with this default on, **any peer you add to `pull_from` can, by
default, type a capped nudge into your live pane.** Accepting cross-store delivery
from a source therefore also grants it a live-pane ping. To narrow or disable it:

- `WEAVE_INJECT_PULLED=false` (or `inject_pulled = false`) — pure queue-only: the
  message still arrives in your inbox on the next drain; only the live nudge is
  suppressed.
- `WEAVE_ALLOW_INJECT_FROM=...` (or `allow_inject_from = [...]`) — restrict the
  nudge to a trusted subset of your pull sources; other sources still deliver to
  your inbox, just without a keystroke.

### Signed sender identity (optional `sign` feature)

The **default build is crypto-free** — no `ed25519-dalek` in its dependency tree.
Cross-store `from` attribution is advisory by default — **unless you configure a
trust set** (see below), in which case a trusted sender is held to strict
verification automatically. You opt into signed identity by building with the
`sign` feature:

```bash
cargo build --release --features sign         # composes with libsql too: --features "libsql sign"
```

A `--features sign` build adds the `weave key` subcommand and verifies signatures
on pull/commit using Ed25519 over the canonical `(from, to, body)`:

```bash
weave key gen --me desktop          # generate a keypair; private key stored 0600, public key + fingerprint registered + printed
weave key show --me desktop         # print this session's public key + fingerprint (never the private key)
weave key fingerprint --me desktop  # print just the SHA256:<16-hex> fingerprint peers add to WEAVE_TRUST (--json)
weave key add envctl <hex-pubkey>   # register a peer's public key so their signed intents verify
weave key list                      # list registered (identity, public key, fingerprint) triples (--json)
weave key rotate --me desktop       # archive the old key (0600), generate a new one, print both fingerprints + overlap guidance
weave key revoke <fingerprint>      # print the value to add to WEAVE_REVOKED to retire a key (config-driven; no store table)
```

The private key lives at `~/.config/weave/ed25519.key` (mode `0600`) and is never
logged or printed. A signed intent makes the cross-store `from` **unforgeable**; a
**tampered or spoofed signature is always rejected** regardless of mode.

#### Fingerprints

A **fingerprint** is `SHA256:` followed by the first **16 hex chars** of the
SHA-256 digest of the **raw 32-byte public key** — short, stable, secret-free, and
derived only from the public key (never the private key). The 16-hex form is for
**display**; trust and revocation always match against the **full** SHA-256 digest
(`SHA256:<64-hex>`) or a full pubkey hex, so a truncated display string can never be
the basis of a trust decision. `weave key rotate` / `weave key revoke` emit the full
`SHA256:<64-hex>` form (the value `WEAVE_TRUST` / `WEAVE_REVOKED` actually match).

#### Trust set, strict-by-default, and revocation

Three receiver-local config keys (all inert without the `sign` feature) govern how a
pulled intent is verified. **Trusting a sender first requires `weave key add
<identity> <pubkey>`** so weave has the key to compute that sender's fingerprint.

- **`WEAVE_STRICT_VERIFY` is tri-state** (env, or `strict_verify` in config):
  - **unset** — the trust-set-aware default: a *trusted* sender is verified
    strictly (unsigned/unverifiable intents from it are **dropped**); every other
    sender keeps the advisory model and still commits unsigned.
  - **`1` / `true`** — force strict **everywhere**: any unsigned/unverifiable
    intent is dropped, trust set or not.
  - **`0` / `false`** — advisory **everywhere**: unsigned/unverifiable intents
    commit even from a trusted sender. This **never re-admits a revoked key's
    signed message** — a signature that verifies against a revoked key is still
    rejected unconditionally (see revocation below).
- **`WEAVE_TRUST`** (env, or `trust = [...]` in config) — a comma- or
  whitespace-separated list of **trusted fingerprints** (`SHA256:<64-hex>`) or full
  pubkey hex strings. Configuring a non-empty trust set is what makes strict the
  default for the senders in it (per the unset case above). Entries are validated,
  deduped, and capped (64).
- **`WEAVE_REVOKED`** (env, or `revoked = [...]` in config) — a list of **revoked
  fingerprints** (same forms). A signature that verifies against a revoked key is
  **rejected unconditionally**, even with `WEAVE_STRICT_VERIFY=0` / advisory mode —
  revocation of a known-bad key is not defeatable by the global toggle.

A **tampered or spoofed (present-but-invalid) signature is always rejected** in
every configuration. The only intents that strict mode drops are *unsigned* or
*unverifiable* ones; a *valid* signature from a non-revoked key always commits.

## Status

v0.1.0 — both backends build clean (clippy `-D warnings`), **38 tests green** (22 unit + 16
integration), MCP + CLI + injector + setup automation working; libSQL backend runtime-verified.
Live pane injection is validated by construction (pure command-builder unit tests + fake-mux
integration test); end-to-end mux injection on real tmux/zellij is to be confirmed on the target box.
