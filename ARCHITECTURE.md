# weave — Architecture

`weave` is a single static Rust binary that lets coding-agent sessions (Claude
Code and friends) message each other over a shared mailbox, and — when a
recipient is running inside a terminal multiplexer — **pushes** the message into
that recipient's live pane via a native injector. No Python, no daemon, no
external dependency on `repowire`.

This document describes the modules, the `Store` trait and its backends, the
native injector, the no-daemon push model, lifecycle-hook auto-delivery, the
data model, the threat model, and how weave compares to the two prior tools on
this box (`mcp-broker` and `repowire`).

---

## 1. Module map

The crate is one binary (`src/main.rs`) with focused modules. Each module owns
one concern and depends only on the layers beneath it.

```
src/
├── model.rs         core types + helpers (no I/O); incl. the Tier-2 Intent
├── config.rs        config file + env overlay
├── sign.rs          OPTIONAL Ed25519 sign/verify + keyfile (cfg(feature="sign"))
├── store.rs         Store trait + bundled SQLite backend
├── store_libsql.rs  feature-gated libSQL/Turso backend (cfg(feature="libsql"))
├── inject.rs        native multi-mux injector (pure command tables + runner)
├── mcp.rs           MCP stdio JSON-RPC 2.0 server (weave_* tools)
├── setup.rs         `weave setup` / `weave uninstall` (MCP register + hook merge)
└── main.rs          clap CLI; wires config → store → {mcp, cli, hooks}
```

Dependency direction (top depends on bottom):

```
main ──▶ mcp ──▶ store ──▶ model
  │       │        │  │      ▲
  │       └────────┴── inject ┘   (inject and mcp both use model::Peer)
  └──▶ config ◀── store          (store calls config::this_host / peer_db_paths)
```

`config` is the lowest layer above `model`: `store`'s liveness (`is_alive` →
`this_host`), federation (`federated_*` → `peer_db_paths`), and Tier-2 delivery
(`pull_from_store` → `pull_from_paths`) read it downward. The direction never
reverses. The optional `sign` module sits just above `config` (it reads the config
dir for the keyfile); `store` depends down on it for verify-on-commit. The Tier-2
consent nudge is fired **caller-side** in `main`/`mcp`, so there is **no
`store → inject` edge** (§10).

### `model.rs` — core types, no I/O

- `Message { id, ts, sender, recipient, subject: Option, body }`
- `Intent { id, ts, to, to_host, from, subject: Option, body, sig }` — a Tier-2
  cross-store delivery intent: a directed message the sender deposits in its **own**
  outbox for a recipient that lives in another store (§10). `id`/`ts` are the
  sender's local values (the receiver dedups on the source `id` and re-stamps `ts`
  on commit); `sig` is the optional Ed25519 signature (empty unless `--features
  sign`).
- `Peer { name, mux, target, cwd: Option, last_seen, pid: Option<i64>, host }` —
  a session that has registered itself plus where (if anywhere) it can be
  injected. `pid`/`host` are captured at registration so liveness can be checked
  by probing the owning process (see §6); both are additive (`#[serde(default)]`)
  so older rows deserialize cleanly.
- `now() -> i64` — UNIX seconds; timestamps are stored as integers so weave
  needs **no date crate**.
- `fmt_ts(i64) -> String` — formats UNIX seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC)
  using Howard Hinnant's civil-from-days algorithm, again avoiding a date crate.
- Broadcast set: `BROADCAST = ["all","*","everyone","broadcast"]`,
  `is_broadcast(&str) -> bool`, and `BROADCAST_SQL` (the same list as a SQL
  literal tuple). `BROADCAST_SQL` is **derived from the same constant list** so
  the Rust check and the SQL filter can never drift. Its values are compile-time
  constants (never user input), so embedding them as SQL literals is safe.

### `config.rs` — configuration

`Config { session, backend, db, nudge_template, libsql_url, libsql_auth_token,
retention_secs, peer_dbs, pull_from, inject_pulled, allow_inject_from,
strict_verify }`, all `Option`. `Config::load()` reads `~/.config/weave/config.toml`
(`$XDG_CONFIG_HOME` honored) if present, then overlays environment variables:
`WEAVE_SESSION`, `WEAVE_BACKEND`, `WEAVE_DB`, `WEAVE_LIBSQL_URL`,
`WEAVE_LIBSQL_AUTH_TOKEN`, `WEAVE_RETENTION_SECS`, `WEAVE_PEER_DBS`,
`WEAVE_PULL_FROM`, `WEAVE_INJECT_PULLED`, `WEAVE_ALLOW_INJECT_FROM`,
`WEAVE_STRICT_VERIFY` (env wins over file; the list vars *union* onto the file
list). Helpers:

- `backend()` → defaults to `"sqlite"`.
- `db_path()` → config/`WEAVE_DB` override, else
  `$XDG_DATA_HOME/weave/messages.db` (default `~/.local/share/weave/messages.db`).
- `default_db_path()` → the XDG default path, used by `doctor` to flag a
  non-default `WEAVE_DB` (the `db_is_default` field).
- `nudge(from, body)` → the live-injection nudge text, from `nudge_template`
  (with `{from}`/`{body}` substituted) or a built-in default that embeds the body.
- `this_host()` → a stable per-machine host label (`$HOSTNAME` → first line of
  `/etc/hostname` → `"local"`), trimmed, control-char-stripped, and capped at
  `MAX_HOST_LEN` (128). Used as the `peers.host` value (§6) so liveness only
  probes a PID for a peer on *this* host.
- `peer_db_paths()` → the validated, deduped, capped (`MAX_PEER_DBS` = 16) list
  of extra read-only store paths for federation (§9), unioned from `peer_dbs` and
  `WEAVE_PEER_DBS`; the local `db_path()` is dropped (no self-federation).
  Default (unset) ⇒ empty ⇒ identical-to-today behavior.
- `pull_from_paths()` → the Tier-2 **delivery** sources (§10), validated/deduped
  with the same discipline and cap (`MAX_PULL_FROM` = 16) as `peer_db_paths`, but
  keyed off the **distinct** `pull_from` list. Default (unset) ⇒ empty ⇒ no
  cross-store delivery.
- `inject_pulled()` → the Tier-2 consent toggle, **defaulting to `true`** (the one
  place the original default-off is intentionally flipped). `false` ⇒ pure
  queue-only delivery.
- `allow_inject_from_paths()` / `inject_allowed_from(&Path)` → the optional finer
  gate narrowing which pull sources may fire the consent nudge; unset ⇒ "same as
  the pull set".
- `strict_verify()` → the Tier-2 signed-identity strictness, **defaulting to
  `false`** (advisory fallback); only consulted on the pull/commit path of a
  `--features sign` build.

### `store.rs` — persistence

Owns the `Store` trait (§2), the bundled `SqliteStore`, the SQL schema, the
`ONLINE_TTL_SECS` presence window (900 s), `is_online(last_seen)`, and the
liveness layer on top of it: `is_alive(peer)` and `pid_alive(pid)` (§6). It also
owns the read-only federation aggregator — `open_readonly`, the
`federated_peers` / `federated_sessions` / `federation_status` free functions, and
the pure `merge_peer_views` / `merge_session_views` dedup/tie-break (§9). For
Tier-2 (§10) it owns the `outbox` / `pull_cursor` / `keys` tables, their additive
trait methods, and the `pull_from_store` / `commit_pulled` free functions that
read each allowed source read-only and commit addressed intents into the local
inbox (owner-only-writes; the consent nudge is fired caller-side, so this layer
takes no `inject` dependency).

### `inject.rs` — native injector

The `Mux` enum, `Target { mux, id }`, environment detection, the pure
per-mux command tables, and the runner. Detailed in §3. Also exposes the pure
`capability(&Target) -> Capability` verdict (`Live` / `RegisteredNotAlive` /
`NotInjectable`) composed from `injectable()` + the liveness probe — this is what
`weave connect` / `weave_connect` report (§4). It adds no new spawn path: the
probe is the existing fail-open `target_alive` and the verdict is a pure value.

### `mcp.rs` — MCP server

A newline-delimited JSON-RPC 2.0 server over stdio implementing `initialize`,
`ping`, `tools/list`, `tools/call`, and empty `resources/list` / `prompts/list`.
It exposes the `weave_*` tools and performs the live nudge-inject on send.
stdout is reserved for protocol frames; **all logging goes to stderr**.
`weave_attach` (zero-restart self-adoption — re-capture the pane and upsert the
caller's own peer row) and `weave_connect` (the §4 capability verdict) sit
alongside the messaging tools; the peers/sessions/doctor tools also surface
read-only federation (§9) when extra stores are configured.

### `setup.rs` — Claude Code wiring

`run(exe)` registers the MCP server (`claude mcp add`) and merges weave's lifecycle hooks into `~/.claude/settings.json` (atomic temp+rename write, one-time backup, idempotent, preserving unrelated hooks); `uninstall()` reverses it. (Legacy note, ignore: "not yet
implemented" line and return `Ok(())`. They exist so the CLI surface
(`weave setup` / `weave uninstall`) is stable while the real implementation —
registering the MCP server and merging lifecycle hooks into
`~/.claude/settings.json` idempotently — lands later. Until then, wiring is
manual (see README).

### `main.rs` — CLI + glue

`clap`-derived CLI. Loads `Config`, opens the store, and dispatches subcommands.
`resolve_me()` resolves this session's identity:
**explicit flag > config/`$WEAVE_SESSION` > basename(cwd)**. `setup`/`uninstall`
are handled before the store is opened (they need no DB).

---

## 2. The `Store` trait and its backends

`Store` is the backend-agnostic, **object-safe** persistence interface, so the
app holds a `Box<dyn Store>` and selects the backend at runtime from config.

```rust
pub trait Store: Send {
    fn send(&self, sender:&str, recipient:&str, subject:Option<&str>, body:&str) -> Result<i64>;
    fn inbox(&self, me:&str, include_read:bool, mark_read:bool, limit:i64) -> Result<(Vec<Message>, i64)>;
    fn unread_count(&self, me:&str) -> Result<i64>;
    fn history(&self, me:&str, peer:Option<&str>, limit:i64) -> Result<Vec<Message>>;
    fn sessions(&self) -> Result<Vec<(String,i64,i64)>>;   // (name, unread, last_ts)
    fn total_messages(&self) -> Result<i64>;
    fn clear_inbox(&self, me:&str) -> Result<usize>;
    fn clear_all(&self) -> Result<i64>;
    fn register_peer(&self, name:&str, mux:&str, target:&str, cwd:Option<&str>) -> Result<()>;
    fn get_peer(&self, name:&str) -> Result<Option<Peer>>;
    fn list_peers(&self) -> Result<Vec<Peer>>;
    fn backend(&self) -> &'static str;
    // Tier-2 cross-store delivery (§10), all additive:
    fn enqueue_intent(&self, to:&str, to_host:&str, from:&str, subject:Option<&str>, body:&str, sig:&str) -> Result<i64>;
    fn list_outbox(&self, for_recipient:&str, since_id:i64, limit:i64) -> Result<Vec<Intent>>;
    fn outbox_all(&self, limit:i64) -> Result<Vec<Intent>>;
    fn pull_cursor_get(&self, source:&str) -> Result<i64>;
    fn pull_cursor_set(&self, source:&str, last_id:i64) -> Result<()>;
    // Tier-2 signed identity (§10), additive (the keys table is always present):
    fn register_key(&self, identity:&str, pubkey:&str) -> Result<()>;
    fn get_key(&self, identity:&str) -> Result<Option<String>>;
    fn list_keys(&self) -> Result<Vec<(String,String)>>;
}
```

Key semantics:

- **`inbox`** returns `(messages, remaining_unread)`. Messages are those whose
  recipient is `me` *or* a broadcast alias, excluding `me`'s own sends, newest
  first internally but returned oldest-first for natural reading. When
  `mark_read` is set, each returned message id is recorded in `reads` for `me`
  inside a transaction.
- **Per-reader read tracking**: a broadcast is delivered exactly once *per
  reader*, because read state lives in `reads(message_id, reader)` rather than a
  flag on the message.
- **`clear_inbox`** marks `me`'s unread as read (non-destructive). **`clear_all`**
  truncates `messages` and `reads` (destructive; the MCP tool requires
  `confirm:true`).
- **`register_peer`** is an upsert keyed on `name`, also refreshing `last_seen`
  (used for presence via `is_online`).

### Backends

| Backend | Module | Default? | Sync model | Use |
|---|---|---|---|---|
| `sqlite` | `store.rs` (`SqliteStore`) | yes | synchronous (rusqlite, bundled) | local mailbox |
| `libsql` | `store_libsql.rs` (`LibsqlStore`) | no — `--features libsql` | async (tokio) | cross-machine / Turso replicas |

`SqliteStore::open` creates parent dirs, opens the file, sets
`busy_timeout=30s`, `journal_mode=WAL`, `synchronous=NORMAL`, and applies the
schema idempotently (`CREATE TABLE IF NOT EXISTS`). The on-disk SQLite format is
**libSQL-compatible**, so the same file is portable between backends with no
migration — the file is the broker.

`open_store()` in `main.rs` picks the backend from `Config::backend()`. Selecting
`libsql` in a binary built without the feature fails with a clear message rather
than silently falling back, so configuration mistakes are loud. The default
build (no features) stays green and pulls in no tokio/libSQL tree.

---

## 3. Native injector design

The injector delivers text into a *running* agent session's terminal pane by
driving the terminal multiplexer (or control-capable terminal) it lives in. It
is weave's own first-class component — **no Python, no repowire**.

### Per-mux command tables

`Mux` enumerates the supported backends, each mapping to a CLI binary and an
environment variable used for detection:

| `Mux` | CLI binary | Detect env var | Target meaning |
|---|---|---|---|
| `Tmux` | `tmux` | `TMUX_PANE` | pane id (e.g. `%3`) |
| `Zellij` | `zellij` | `ZELLIJ_SESSION_NAME` | session name |
| `Kitty` | `kitten` | `KITTY_WINDOW_ID` | window id |
| `Wezterm` | `wezterm` | `WEZTERM_PANE` | pane id |
| `Screen` | `screen` | `STY` | session |
| `None` | — | — | not injectable |

`commands_for(target, text) -> Vec<Vec<String>>` is a **pure function**: given a
target and text it returns the exact argv vectors to run, with no side effects
and no multiplexer required. That purity is what makes the injector unit-testable
on a build host with no mux present — every backend has a test asserting its
exact argv, and there are 38 tests total across the crate (22 unit + 16 integration).

`detect_target()` probes the environment most- to least-specific (tmux first,
because a process can be inside tmux *and* a terminal, and the multiplexer owns
the input line) and returns a `Target`. `Target::injectable()` is true only when
the mux is not `None` and the id is non-empty. `Target::from_peer(&Peer)` rebuilds
a target from a registered peer's stored `(mux, target)`.

### Paste-safe submission

Submitting the message (pressing "Enter") is the subtle part. Modern TUIs such
as Claude Code run in **bracketed-paste** mode, where a naive Enter after literal
text can be swallowed or misread as a TUI key (this was a documented `repowire`
bug — injection triggering a cancel mid-tool-call). Each backend therefore uses
the paste-safe submission idiom for its terminal:

- **tmux** — three commands: send the literal text
  (`send-keys -t <pane> -l -- <text>`), then **close bracketed paste** with the
  hex sequence `ESC [ 2 0 1 ~` (`send-keys -t <pane> -H 1b 5b 32 30 31 7e`), then
  send `Enter`. Closing the paste before Enter is what stops the TUI from
  treating the newline as a cancel.
- **zellij** — write the literal chars (`action write-chars <text>`), then write
  byte 13 (`action write 13`).
- **kitty** — match the target window by id and send the text, then send a
  carriage return as a separate `send-text`.
- **wezterm** — `send-text --no-paste` avoids bracketed paste entirely, then
  submit with a carriage return.
- **screen** — `-X stuff "<text>\r"` injects the string plus carriage return in
  one shot.

### Running and graceful degradation

`inject(target, text) -> Result<bool>`:

- returns `Ok(false)` when the target is not injectable (mux `None` / empty id);
- checks the mux binary is on `PATH` via `have(bin)` and `bail!`s with a clear
  message if missing;
- runs each command in order and `bail!`s if any exits non-zero (e.g. the pane
  is gone).

Errors are never fatal to messaging: the message is already persisted, so
callers treat an injection failure as "fall back to next-turn delivery."

---

## 4. The no-daemon push model

weave has **no long-running process**. Push works because every supported
multiplexer can target an arbitrary pane/session from *any* process:

- `tmux send-keys -t <pane>` reaches any pane on the tmux server;
- `zellij --session <name> action write-chars` reaches any zellij session;
- kitty/wezterm/screen have equivalent addressable-target CLIs.

So the **sender injects directly into the recipient's registered pane** — there
is no relay and no broker process in the middle. The `peers` table is the
registry that maps `name → (mux, pane/session id)`, captured from the
environment (`$TMUX_PANE`, `$ZELLIJ_SESSION_NAME`, etc.) at `SessionStart`.

**Registration / adoption seam.** Registration is an upsert keyed on the
session's *own* resolved identity, so it can run at three moments:
`weave hook session` at `SessionStart`, the explicit `weave register`, and
`weave attach` / `weave_attach` — the **zero-restart adoption** path. A session
that started outside a multiplexer (or before `weave setup`) can re-capture its
current pane and upsert its own row at any time without restarting; the upsert
binds the caller's own validated identity, so there is no argument path to
overwrite another peer's row. All three capture the process `pid` + `host`
(§6) for liveness.

**Connect handshake.** Before sending, a caller can probe reachability with
`weave connect --to <peer>` / `weave_connect`. It looks up the peer, builds a
`Target`, and reports the pure `inject::capability()` verdict — `Live`,
`RegisteredNotAlive`, or `NotInjectable`. This **reuses the existing injector**
(no new injector, no new spawn path): the verdict is computed from `injectable()`
plus the fail-open liveness probe. A not-alive or non-injectable verdict is **not
an error** — those messages still arrive via the recipient's next store drain;
only a non-existent peer is an error.

Send path (MCP `weave_send`, mirrored by the `weave send` CLI):

1. `store.send(from, to, subject, body)` persists the message and returns its id.
2. If `to` is **not** a broadcast and `store.get_peer(to)` yields an injectable
   peer, weave builds a `Target::from_peer` and calls `inject()` with the nudge
   text (default `[weave] message from <from>: <body> (run weave_inbox to read)`,
   or the configured `nudge_template` with `{from}`/`{body}` substituted). The
   nudge carries the message body so the recipient sees the content the instant
   it lands; the persisted copy still arrives on their next hook drain.
3. The tool result reports whether a live nudge was injected, whether the peer
   had no injectable target, or that inject failed and the message will arrive on
   the recipient's next turn.

Broadcasts are never injected (only persisted) — they fan out to every reader via
inbox/hook delivery, not by pushing into N panes.

The DB is the only shared state. Concurrent senders are serialized by SQLite's
WAL mode + busy timeout. A presence daemon (`weaved`) is explicitly **optional
future work**, needed only for live online/offline status and lifecycle eviction
— not for messaging or injection.

---

## 5. Lifecycle-hook auto-delivery

When weave is wired into Claude Code's lifecycle hooks, the CLI subcommand
`weave hook <event>` runs at session events. Each hook reads the event JSON on
stdin (for `cwd`), resolves the session identity, and acts:

| Hook event | Claude Code trigger | Action |
|---|---|---|
| `session` | `SessionStart` | `detect_target()` + `register_peer_full(name, mux, id, cwd, pid, host)` — the session becomes an injectable peer, capturing its PID + host for liveness (§6). |
| `prompt` | `UserPromptSubmit` | Drain unread (`inbox` with `mark_read`) and print each to **stdout**, which Claude Code folds into the agent's context. |
| `stop` | `Stop` | Same drain as `prompt`. |
| `notification` | `Notification` | Reserved for future use (no-op today). |

This is the **graceful-degradation** path: even with no multiplexer present (so
no live injection is possible), unread messages are still delivered into the
agent's context on its next turn or when it stops. The two delivery channels
compose — an injectable peer gets an instant nudge *and* the full message on its
next hook drain; a non-injectable session gets only the hook drain.

---

## 6. Data model

Tables created idempotently (`CREATE TABLE IF NOT EXISTS`):

```sql
messages    (id INTEGER PK AUTOINCREMENT, ts INTEGER, sender TEXT, recipient TEXT,
             subject TEXT NULL, body TEXT)
reads       (message_id INTEGER, reader TEXT, ts INTEGER, PRIMARY KEY(message_id, reader))
peers       (name TEXT PRIMARY KEY, mux TEXT, target TEXT, cwd TEXT NULL,
             last_seen INTEGER, pid INTEGER NULL, host TEXT NOT NULL DEFAULT '')
-- Tier-2 cross-store delivery (§10):
outbox      (id INTEGER PK AUTOINCREMENT, ts INTEGER, to_peer TEXT, to_host TEXT NOT NULL DEFAULT '',
             from_peer TEXT, subject TEXT NULL, body TEXT, sig TEXT NOT NULL DEFAULT '')
pull_cursor (source TEXT PRIMARY KEY, last_id INTEGER NOT NULL)
keys        (identity TEXT PRIMARY KEY, pubkey TEXT NOT NULL)
```

- **`messages`** — the append-only mailbox. `recipient` is a session name or a
  broadcast alias.
- **`reads`** — per-`(message, reader)` read state. This is what makes a
  broadcast deliverable exactly once per reader and keeps each session's "unread"
  independent.
- **`peers`** — the injection registry: where each named session can be reached,
  plus `last_seen`, `pid`, and `host` for presence. The `pid`/`host` columns are
  added by an **additive, idempotent migration** (guarded, mirroring the `socket`
  precedent) in **both** backends, so a pre-existing DB upgrades in place and an
  old row reads `pid:NULL` / `host:""`.
- **`outbox`** — Tier-2 pending intents the owner queued for recipients in *other*
  stores (§10). Append-only; `id` is the monotonic dedup key the receiver tracks.
  `sig` is empty unless `--features sign` signed the intent.
- **`pull_cursor`** — the receiver's per-source high-water mark on the source's
  `outbox.id`, the idempotency key for pull/commit.
- **`keys`** — registered `(identity, public key)` pairs for signed-identity
  verification (always present plain data; the SIGN/VERIFY crypto is `sign`-gated).
- The three Tier-2 tables are whole **new** tables created on every open in **both**
  backends, so a legacy (pre-Tier-2) DB upgrades in place with no per-column ALTER.

### Presence: `is_alive` vs `is_online`

`is_online(last_seen)` is the pure recency guard (within `ONLINE_TTL_SECS` =
900 s). **Presence display now means *alive*, not "wrote recently":**

```text
is_alive(peer) = is_online(peer.last_seen)
               ∧ match peer.pid {
                     Some(pid) if peer.host == this_host() => pid_alive(pid),
                     _                                      => true,   // fail open
                 }
```

- A peer on **this host** with a known PID is confirmed by probing the process;
  `pid_alive` is a Linux `/proc/<pid>` existence check (no new dependency) and
  **degrades to assume-alive** off Linux via `cfg`.
- The probe **fails open** for a remote / cross-machine peer (`host != this_host()`,
  e.g. a Turso/libSQL shared DB) or an unknown PID (`pid:NULL`) — a peer we cannot
  probe must never read dead. So a dead local session drops from presence the
  moment its process exits, while a remote peer still relies on the TTL.

Read paths keep `last_seen` warm: `weave peers` and a long-lived `weave watch`
each refresh presence (heartbeat-on-read, explicit-identity only) so a session
stays visible even with no message traffic.

Unread for `me` = messages addressed to `me` or to a broadcast alias, not sent by
`me`, with no matching `reads` row for `me`. Timestamps are UNIX seconds
(`model::now()`), formatted to UTC ISO-8601 only at display time
(`model::fmt_ts`).

---

## 7. Threat model

weave runs locally and trusts the operator of the machine; its mailbox is a
local file readable by that user. The security focus is therefore on **how
injected and stored text is handled**, not on network attackers.

- **No shell, ever.** Every external command is spawned with
  `std::process::Command::new(bin).args(...)` — an explicit argv vector. weave
  never builds a shell command string and never invokes `sh -c`, so message
  bodies and session names cannot be interpreted as shell syntax. There is no
  command-injection surface even if a message body contains `;`, `$(...)`,
  backticks, or quotes.
- **Argument handling is structured.** `commands_for()` places user text as a
  *single argv element* (e.g. tmux's `-l -- <text>` literal mode, where `--`
  ends option parsing so a body starting with `-` is not mistaken for a flag).
  Pure construction means the exact bytes that reach the mux CLI are
  unit-asserted.
- **SQL is parameterized.** All variable values use bound `params!`. The only
  inlined SQL literals are the broadcast aliases, which are compile-time
  constants derived from `BROADCAST` — never user input — so `BROADCAST_SQL`
  cannot be an injection vector.
- **Injection is a contained side effect.** The worst case of a hostile body is
  the text appearing in another session's pane (a social/UX concern), not code
  execution. A failed or impossible injection degrades to next-turn hook
  delivery; it never crashes the sender, because the message is already
  persisted before injection is attempted.
- **stdout discipline.** The MCP server writes only protocol frames to stdout and
  all diagnostics to stderr, so a malformed log line can't corrupt the JSON-RPC
  stream.
- **Destructive ops are gated.** `weave_clear` with `scope:"all"` wipes every
  session's messages and requires an explicit `confirm:true`; the default scope
  only marks the caller's own inbox read.
- **Identity is advisory.** Session names are free strings with no
  authentication — appropriate for a single-user local mesh. weave does not
  defend one local session against another impersonating it; that is out of
  scope for the local-trust model (and would be the job of a future relay tier).
- **Cross-store access is read-only (Tier-1).** Federation (§9) opens foreign
  stores with `SQLITE_OPEN_READ_ONLY` and never writes them, so aggregating
  another project's peers/sessions cannot mutate that project's store and stays
  inside the single-local-trust-domain assumption.
- **Owner-only-writes (Tier-2).** Cross-store delivery (§10) never lets store A
  write store B. A sender deposits a directed *intent* into its **own** outbox; the
  recipient pulls each allowed source **read-only** (`open_readonly`,
  `SQLITE_OPEN_READ_ONLY`, no schema/migrate/harden) and commits the intents
  addressed to it into its **own** inbox. Every write the pull driver performs
  (`Store::send`, `pull_cursor_set`) targets the *local* store — the source is
  never written, migrated, or created. This is a first-class structural invariant
  (the storage engine rejects any write to the read-only handle), proven by a
  byte-unchanged-source test on both backends. It is what keeps "identity is
  advisory" acceptable across stores: store A cannot mutate store B, so the only
  thing cross-store carries is data B chooses to pull and commit itself.
- **Signed sender identity is optional (Tier-2, `sign` feature).** By default
  cross-store `from` is advisory, exactly like a same-store send. A `--features
  sign` build (Ed25519, §10) makes a signed `from` unforgeable and **always**
  rejects a tampered or spoofed signature; the default build links no crypto.

---

## 8. Comparison to mcp-broker and repowire

weave is the third iteration of inter-session messaging on this box, built to
keep what worked and drop the operational weight.

| | `mcp-broker` | `repowire` | **weave** |
|---|---|---|---|
| Language / footprint | Python + libSQL (uv venv) | Python (uv tool) | **Rust, one static binary** |
| Push to a running session | ❌ poll-only | ✅ via daemon | ✅ **sender injects directly** |
| tmux injector | n/a | ✅ | ✅ |
| Native zellij injector | n/a | ❌ | ✅ |
| Other muxes | n/a | ❌ | ✅ kitty / wezterm / screen |
| Daemon required | no | **yes** (127.0.0.1:8377) | **no** (optional later) |
| Paste-safe submission | n/a | partial (had cancel bug) | ✅ per-mux idiom |
| MCP-native | ✅ | ✅ | ✅ |
| Storage | libSQL DB | service state | libSQL-compatible SQLite file |
| Cross-machine / Telegram | ❌ | ✅ | ❌ (non-goal for now) |

- **mcp-broker** (`broker_send/inbox/history/sessions/clear`, libSQL, runs under
  a uv-managed CPython) proved the broker semantics weave adopts — per-reader
  read tracking, `to:"all"` broadcast, history, sessions, clear-with-confirm —
  but is **poll-only**: a running session is never flagged; it sees a message
  only when it next calls `broker_inbox`.
- **repowire** added real push (peer registry + tmux pane injection + Claude
  lifecycle hooks) but at the cost of a Python runtime, a **long-running daemon**,
  and a large product surface (relay, Telegram, dashboard). It is **tmux-first
  with no native zellij injector**, which matters because this box's daily shell
  is zellij.
- **weave** keeps the broker semantics and the push, drops the daemon (the mux
  CLIs reach any pane from any process, so the *sender* injects) and the Python
  runtime, and ships a **native multi-mux injector** (tmux + zellij first-class,
  plus kitty/wezterm/screen) in a single dependency-free binary. Cross-machine
  relay and chat bridges are deliberate non-goals for now; the libSQL-compatible
  on-disk format leaves a clean path to Turso replicas if cross-machine ever
  becomes a real need.

---

## 9. Read-only multi-store federation (Tier-1)

A session normally sees only its own `WEAVE_DB`. Federation lets `weave peers` /
`weave sessions` (CLI and the matching MCP tools) **aggregate peers and sessions
across several stores read-only**, so an agent can see sessions living in other
projects' mailboxes without those projects sharing one DB.

- **Configuration.** `WEAVE_PEER_DBS` (comma- or path-list-separated) and/or
  `peer_dbs = [...]` in `config.toml` list extra store files. They are unioned,
  validated, deduped, the local store is dropped (no self-federation), and the
  list is capped at `MAX_PEER_DBS` (16). Unset ⇒ empty ⇒ behavior identical to
  a single-store run (the listings are byte-identical).
- **Read-only by construction.** Each foreign store is opened via
  `open_readonly` (`SQLITE_OPEN_READ_ONLY`, no `CREATE`, no schema, no migration,
  no permission hardening) on **both** backends — the storage engine rejects any
  write, so the guarantee is structural, not a convention. libSQL 0.9 exposes the
  same `SQLITE_OPEN_READ_ONLY` open, so neither backend is gated off.
- **Aggregation + dedup.** `federated_peers` / `federated_sessions` open each
  extra store read-only, list it, and feed the rows through the pure
  `merge_peer_views` / `merge_session_views`. Peers dedup on `(name, host)`
  (tie-break: alive > not-alive, then newer `last_seen`, then local origin);
  sessions dedup on `name`, keeping `max(last_activity)` and **never summing
  unread** (a foreign store's unread is not in this session's local inbox — Tier-1
  has no cross-store inbox). Presence reuses §6 `is_alive` unchanged (a foreign
  peer on a different host fails open to TTL).
- **Origin tagging.** Foreign rows are tagged ` (via <store-label>)` in text and
  carry additive `origin` / `foreign` fields in `--json`; local rows are
  unchanged (regression-safe). `doctor` reports configured / ok / skipped store
  counts.
- **Failure isolation.** An unreadable / missing / non-weave / locked extra store
  is **skipped** — a note goes to stderr (MCP keeps stdout clean) and the local
  listing still returns with exit 0. One bad path never breaks the command.

**Tier-1 is read-only aggregation.** It can never deliver a message into your
inbox; `pull_from` (a strictly higher trust grant) does that — see §10. A path may
appear in both lists; adding a store to `peer_dbs` to *view* it never silently
upgrades it into a *delivery* source.

---

## 10. Cross-store delivery (Tier-2)

Tier-2 lets sessions in **different stores** message each other without sharing one
`WEAVE_DB`, using a broker-mediated **request-pull** model (Option C) in which the
DB files are the only shared state and **only a store's owner ever writes it**
(§7 owner-only-writes).

### The flow

1. **Send (owner of A).** `weave send --to-store <B-store> --to <name>` (or
   `weave_send` with `to_store`) writes an `Intent` into **A's own** `outbox`. B's
   store is never opened on the send path. A cross-store broadcast is refused
   (directed delivery only). `weave outbox` / `weave_outbox` inspect A's pending
   intents read-only.
2. **Pull/commit (owner of B).** B lists A among its delivery sources
   (`WEAVE_PULL_FROM` / `pull_from = [...]`, distinct from `peer_dbs`). On each
   drain (the `prompt`/`stop` hook, `weave watch`, the MCP `weave_inbox` drain) or
   an explicit `weave pull`, B opens each allowed source **read-only**, reads the
   intents addressed to it since its per-source cursor, and commits each into its
   **own** inbox via the normal local `Store::send` (so B assigns the id and
   timestamp). It then advances `pull_cursor` for that source.

### Idempotency + the at-least-once contract

The dedup key is the **source's `outbox.id`** (`AUTOINCREMENT`, append-only ⇒
monotonic), recorded per source in `pull_cursor(source, last_id)`; a pull reads
only `id > last_id`. A normal re-drain therefore **never duplicates**. The cursor
is advanced **after each commit** (not one batch transaction — friendlier to the
async libsql path). The only re-delivery window is a crash *between* committing a
message and advancing the cursor, which on the next drain re-delivers **at most one
intent** — a **bounded, single-intent at-least-once** guarantee, not whole-batch
replay. A misaddressed or malformed intent is skipped and the cursor still advances
past it, so one poison row cannot wedge a source. Each drain is bounded to
`MAX_PULL_PER_DRAIN` intents per source (DoS guard).

### Consent nudge on a pulled message — DEFAULT ON

When B commits a message from an **allow-listed** source, B also fires the existing
content-free, paste-safe `Nudge::Nudge` (a fixed "check your inbox" ping) into
**B's own** registered pane, by default (`inject_pulled` defaults to `true`). The
body is never in the keystroke; only B's own pane is ever touched (never a foreign
pane); A has no injection path at all. Gating, in order: (1) `inject_pulled` off ⇒
queue-only; (2) the committing source must pass `inject_allowed_from`
(`allow_inject_from` narrows the inject set to a subset of the pull set; unset ⇒
"same as the pull set"); (3) B must have its own registered, injectable, live pane,
else it falls open to queue-only. **Residual risk:** with the default on, any source
on B's pull/allow set can type a capped nudge into B's live pane — accepting
delivery from a source also grants it a live-pane ping. `WEAVE_INJECT_PULLED=false`
disables it; `WEAVE_ALLOW_INJECT_FROM` narrows it.

The nudge is fired **caller-side** (`main::nudge_pulled`, `mcp::nudge_pulled`),
exactly where the live-send nudge already lives — in modules that already depend on
both `store` and `inject`. The pull driver (`pull_from_store`, a `store`-layer free
function) stays inject-free: it only **records** which source paths committed
(`Pulled.committed_sources`) so the caller can gate per source. No new
`store → inject` edge is introduced; the layering DAG is unchanged.

### Signed sender identity — optional `sign` feature

By default the cross-store `from` is advisory. Building with `--features sign`
(Ed25519 via `ed25519-dalek`, mirroring the `libsql` optional-dependency pattern —
the **default build links no crypto**) adds verifiable identity:

- A new low, pure `sign` module (depends only on `config` + std) owns the canonical
  encoding, sign/verify, hex codec, and the keypair file. The private key lives at
  `~/.config/weave/ed25519.key` (mode `0600`), is never logged or printed, and
  refuses to clobber an existing key.
- The canonical signature covers `(from, to, body)` — **not** `created`/`ts`, which
  is advisory and re-stamped by the receiver on commit, so binding it would be a
  fragile coupling with no integrity gain. Length-prefixed with a
  domain-separation prefix so no field boundary is ambiguous.
- A new `keys(identity, pubkey)` table (always present, plain data, both backends)
  stores peers' public keys. `weave key gen|show|add|list` (subcommand present only
  under `--features sign`) manages them.
- **Sign on enqueue** (A signs its outbound intent if it has a key);
  **verify on commit** (B, before its local write): a present-but-invalid signature
  (tamper/spoof) is **always rejected**; a valid one makes `from` unforgeable; an
  unsigned intent — or one with no registered key to check against — falls back to
  the advisory model and commits, **unless** `strict_verify` (`WEAVE_STRICT_VERIFY`)
  is set, which drops it. Verification reads only B's own `keys` table; the source
  is still opened read-only (owner-only-writes intact).

`sign` is a low module (`model ← config ← sign`); `store` depends down on it for
verify-on-commit; `main`/`mcp` depend down on both. No upward edge.
