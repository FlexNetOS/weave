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
- `strict_verify_override()` → the **tri-state** Tier-2 signed-identity strictness
  override (`Some(true)` = force strict everywhere, `Some(false)` = advisory
  everywhere for the unsigned/unknown path, `None` = no override ⇒ the
  trust-set-aware default decides per sender). `trust_set()` / `revoked_set()` →
  the validated, deduped, capped (`MAX_TRUST` = 64) receiver-local fingerprint
  lists; `trust_set_configured()` → whether the trust set is non-empty (which makes
  strict the default for trusted senders). All four are only consulted on the
  pull/commit path of a `--features sign` build; the collapsed-bool `strict_verify()`
  is retained for back-compat. Default (everything unset) ⇒ advisory ⇒
  identical-to-today.

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
    // Tier-2 signed identity (§10), additive (the identity_keys table is always present):
    fn register_key(&self, identity:&str, pubkey:&str) -> Result<()>; // APPENDS (multi-key)
    fn get_key(&self, identity:&str) -> Result<Option<String>>;        // most-recent shim
    fn get_keys(&self, identity:&str) -> Result<Vec<String>>;          // ALL keys, oldest-first
    fn remove_key(&self, identity:&str, pubkey:&str) -> Result<bool>;  // prune a retired key
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

### Session tag acquisition (`src/git.rs`)

Where `detect_target()` answers *how to reach* a session, `src/git.rs` answers
*what the session is*: it derives the session's **repo name**, **branch**, and a
canonical **worktree id** from its cwd at registration (and refreshes them on
`weave scan`), so a `peers` row is self-describing across the mesh. It sits at the
inject tier — pure parsers plus a no-shell argv `git` runner — and is **never** a
build/link dependency: `git` is invoked as an **external trusted binary** (resolved
via `inject::resolve_trusted`, an absolute path from a trusted dir, never ambient
`$PATH`), so weave stays one dependency-light Rust binary.

Acquisition is best-effort and total — a git/fs failure or a non-git cwd yields
empty tags and **never sinks registration** (the hook hot path) — and writes are
self-only (owner-only-writes: a session only ever tags its own row):

- **`worktree_id`** comes from a pure `.git`-file parse *first* (zero subprocess):
  a linked worktree's `<cwd>/.git` is a file holding
  `gitdir: …/.git/worktrees/<name>/.git`, and `parse_worktree_id_from_gitdir`
  recovers `<name>` as the canonical id. A main (non-linked) worktree has a `.git`
  *directory* → the literal `"(main)"` sentinel. No `.git` at all → empty (and the
  subprocess is skipped entirely).
- **`branch`** is `git rev-parse --abbrev-ref HEAD`, with a
  `git worktree list --porcelain` parse fallback (`parse_worktree_porcelain`,
  matched to this worktree's path) when the former is blank/detached.
- **`repo`** is the basename of `git rev-parse --show-toplevel`
  (`repo_name_from_toplevel`).

The argv `git` runner (`git_capture`) copies `inject::run_capture`'s discipline:
`Command::new(<trusted git>).args([...]).current_dir(cwd)` with a wall-clock
timeout + kill and `Stdio::null()` stderr/stdin — explicit argv, never `sh -c`,
never a built command string, so cwd/repo/branch text never reaches a shell. The
store seam (`sanitize_tag`, §7) bounds and control-strips every tag on write.

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
asks        (id TEXT PRIMARY KEY, question_msg_id INTEGER NOT NULL, answer_msg_id INTEGER NULL,
             asker TEXT NOT NULL, askee TEXT NOT NULL, subject TEXT NULL,
             state TEXT NOT NULL, reply_to TEXT NULL, close_note TEXT NULL,
             opened_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL, closed_ts INTEGER NULL,
             parent_id TEXT NULL)                                  -- ask-many child link (P2, additive)
ask_groups  (parent_id TEXT PRIMARY KEY, asker TEXT NOT NULL, subject TEXT NULL,
             body TEXT NOT NULL, opened_ts INTEGER NOT NULL, target_count INTEGER NOT NULL)
peers       (name TEXT PRIMARY KEY, mux TEXT, target TEXT, cwd TEXT NULL,
             last_seen INTEGER, pid INTEGER NULL, host TEXT NOT NULL DEFAULT '',
             repo TEXT NOT NULL DEFAULT '', branch TEXT NOT NULL DEFAULT '',
             worktree_id TEXT NOT NULL DEFAULT '')
-- Tier-2 cross-store delivery (§10):
outbox      (id INTEGER PK AUTOINCREMENT, ts INTEGER, to_peer TEXT, to_host TEXT NOT NULL DEFAULT '',
             from_peer TEXT, subject TEXT NULL, body TEXT, sig TEXT NOT NULL DEFAULT '')
pull_cursor   (source TEXT PRIMARY KEY, last_id INTEGER NOT NULL)
keys          (identity TEXT PRIMARY KEY, pubkey TEXT NOT NULL)   -- DEPRECATED shadow (#7)
identity_keys (identity TEXT NOT NULL, pubkey TEXT NOT NULL, added_ts INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (identity, pubkey))                    -- multi-key registry (#7)
revocations   (id INTEGER PK AUTOINCREMENT, ts INTEGER NOT NULL, fp TEXT NOT NULL,
               identity TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT '',
               kind TEXT NOT NULL DEFAULT 'enforced')             -- observed-revocation audit log (#11)
```

- **`messages`** — the append-only mailbox. `recipient` is a session name or a
  broadcast alias.
- **`reads`** — per-`(message, reader)` read state. This is what makes a
  broadcast deliverable exactly once per reader and keeps each session's "unread"
  independent.
- **`asks`** — the **tracked ask/answer/ack** side-table (P1, the first step toward
  weave⊇repowire capability parity). Like `reads` and `revocations`, it is a **mutable
  side-table to the append-only `messages`**: one row per correlation-tracked request,
  keyed by an opaque `correlation_id` (`ask_<rowid>_<nonce>`, minted server-side, charset
  `[A-Za-z0-9_]`, validated by `model::ask_id_valid` before any bind). The actual
  question/answer **text reuses `messages`** (threaded via `in_reply_to`) — `asks` holds
  only the correlation + lifecycle (`asker`/`askee`/`subject`, the `state`, pointers
  `question_msg_id`/`answer_msg_id`, an optional `reply_to` chain link, the `close_note`,
  and `opened`/`updated`/`closed` timestamps) and points at the `messages` rows by id.
  `state` is a **monotonic** lifecycle, `open → answered → acked` (never backward),
  enforced by the pure `model::AskState::can_transition` *before* any UPDATE — an illegal
  edge (double-ack, answering an acked thread) is a clean `bail!`, never a panic or a
  silent regression. `ask(reply_to = X)` acks `X` and links the new question into `X`'s
  conversation in the same transaction (so `weave thread` renders the chain). The live
  nudge + the honest delivery verdict (`transport_delivered` / `queued_next_turn` /
  `recipient_not_injectable`) are computed **caller-side** in `mcp`/`main` by reusing the
  existing `inject::capability` + `inject_mode` return — there is **no `store → inject`
  edge** and **no new dependency**. Always-present plain data; point-to-point only
  (broadcast/cross-store ask are out of P1). Created on every open in **both** backends
  via an additive, guarded, idempotent migration (the `reads`/`revocations` precedent).
- **`ask_groups`** + **`asks.parent_id`** — the **ask-many** parent↔child link (P2, the
  second weave⊇repowire parity epic). `ask_many` fans **one question to an explicit list
  of peers**: it inserts one `ask_groups` parent anchor (`askm_<id>`, validated by
  `model::ask_many_id_valid`) holding the canonical question `body`/`subject`/`asker` and
  the de-duped `target_count`, then creates **one normal P1 `ask` per peer** carrying the
  parent's id in the additive nullable `asks.parent_id` column (NULL for every plain ask
  and every legacy P1-era row). A child **is** a P1 ask — it answers/acks through the
  unchanged `open → answered → acked` lifecycle, no duplicated state machine. `ask_many` is
  **best-effort**: an invalid/unreachable/broadcast peer is skipped with a per-child error
  rather than failing the whole call (it never gets a child row and counts as `failed` at
  read time, so `target_count` keeps the totality `answered + acked + pending + failed ==
  target_count` checkable). The per-child live nudge is fired **caller-side** in `mcp`/`main`
  (the same honest-verdict seam as P1) — still **no `store → inject` edge**. `ask_many_result`
  is a **read-time aggregate**: it enumerates the children `WHERE parent_id = ?1`, rolls up
  their states, lists the pending peers, and classifies `complete | partial | pending` via the
  pure `model::classify_ask_many` — **no background ticker, no stored deadline**. `partial`
  appears only when the caller passes an `age` threshold and an open child has waited at least
  that long. The fan-out is bounded by `MAX_ASK_MANY_TARGETS = 64` (explicit peer list only;
  circles compose in a later epic). Both `ask_groups` and `asks.parent_id` ship as **additive,
  guarded, idempotent migrations in both backends** — a legacy P1-era DB upgrades in place
  (`ADD COLUMN parent_id` defaults NULL; `CREATE TABLE IF NOT EXISTS ask_groups`) with **no
  new dependency**.
- **`peers`** — the injection registry: where each named session can be reached,
  plus `last_seen`, `pid`, and `host` for presence, and the descriptive session
  tags `repo` / `branch` / `worktree_id`. The `pid`/`host` and the
  `repo`/`branch`/`worktree_id` columns are each added by an **additive, idempotent
  migration** (guarded, mirroring the `socket` precedent) in **both** backends, so
  a pre-existing DB upgrades in place and an old row reads `pid:NULL` / `host:""` /
  empty tags. The three tag columns are `TEXT NOT NULL DEFAULT ''` (nullable in
  spirit — empty means "unknown/non-git"), appended after `host` at fixed positions
  8/9/10 so the column order is identical across backends. They are **descriptive
  tags only**, never injection targets.
- **`outbox`** — Tier-2 pending intents the owner queued for recipients in *other*
  stores (§10). Append-only; `id` is the monotonic dedup key the receiver tracks.
  `sig` is empty unless `--features sign` signed the intent.
- **`pull_cursor`** — the receiver's per-source high-water mark on the source's
  `outbox.id`, the idempotency key for pull/commit.
- **`identity_keys`** — the multi-key registry (#7): registered `(identity, pubkey)`
  pairs for signed-identity verification, holding **multiple** keys per identity so
  rotation can OVERLAP (old + new both verify during a window). `added_ts` orders the
  keys (newest-first for the `get_key` shim, oldest-first for `get_keys`/`list`).
  Always-present plain data; the SIGN/VERIFY crypto is `sign`-gated. Created on every
  open in **both** backends via an **additive, guarded, idempotent** migration that
  also copies any legacy single-key `keys` rows in (`INSERT OR IGNORE … SELECT identity,
  pubkey, 0 FROM keys`, keyed on the `(identity,pubkey)` primary key so a re-run is a
  clean no-op). A NEW key is APPENDED (`ON CONFLICT(identity,pubkey) DO NOTHING`); a
  duplicate is a no-op; the per-identity count is capped at `MAX_KEYS_PER_IDENT` (16,
  a store constant — bounds a hostile registry; a duplicate never counts against it,
  and exceeding it returns an error, never a panic).
- **`keys`** — the **deprecated** legacy single-key table (`identity PRIMARY KEY`),
  RETAINED as a shadow (no DROP) for crash-safety and old-binary coexistence. Nothing
  reads it anymore; new writes go ONLY to `identity_keys`.
- **`revocations`** — the **observed-revocation audit log** (#11): an append-only
  record of *when* revocation was exercised, for operator visibility only. A
  `declared` row is written when an operator runs `weave key revoke`; an `enforced`
  row is written (best-effort) when the R1 predicate rejects a pulled signed intent
  that verified only against a revoked key. **Write-on-enforce, never read by the
  verifier** — `verify_pulled_intent` never touches this table, so R1 stays the
  single, absolute, config-driven decision source and the log can never weaken or
  drift from it. An audit-write failure is logged to stderr and swallowed; it cannot
  change the rejection. Always-present plain data; every read/write call site is
  `sign`-gated. Created on every open in **both** backends via an additive, guarded,
  idempotent migration (mirroring `identity_keys`). Secret-free: it stores only full
  fingerprints (`SHA256:<64-hex>`, derived from public keys), public identities,
  source labels, and a `kind`. Surfaced read-only by `weave audit revocations` and
  the (count-only) `doctor` / `weave_doctor` verify summary.
- The Tier-2 tables are whole **new** tables created on every open in **both**
  backends, so a legacy (pre-Tier-2) DB upgrades in place with no per-column ALTER;
  `identity_keys` additionally absorbs the legacy `keys` rows on first open.

### Presence: `liveness_for` / `is_alive` vs `is_online`

`is_online_at(last_seen, now_ts)` is the pure recency guard (within
`ONLINE_TTL_SECS` = 900 s, the single freshness window — there is no separate
presence const). **Presence display now means *alive*, not "wrote recently".**

#### A2 — fail-open by host (named principle)

Presence is governed by one rule the tests cite as **A2**: liveness is
**pid-authoritative on the same host, TTL-only (fail-open) on a remote host**.
weave can probe a process only on the machine it runs on, so:

- **Same host** (`peer.host == this_host()`) with a known PID → the PID is
  authoritative: a dead-but-recent local process reads stale.
- **Remote host** (`peer.host != this_host()`, *including an empty host* — see
  below) → **never pid-probed**; weave fails OPEN to the TTL recency verdict (the
  Turso/libSQL shared-DB case). A remote/legacy peer must never falsely read dead,
  and we never probe a PID that might collide with an unrelated local process.

This is a security/correctness invariant, not a heuristic: there is **no
cross-machine pid/network/ssh/ping probe anywhere** — the only probe is the
same-host `/proc/<pid>` check, gated to the local arm. An *empty* host always
classifies remote because `this_host()` is never empty (it falls back to
`"local"`), so `"" != this_host()` holds and the empty-host row fails open by TTL.

#### `liveness_for` — the pure host-aware classifier

The A2 rule lives in one **pure** function in `store` that takes `this_host` and
`now_ts` as parameters (so it is exhaustively testable with a fixed host/clock —
the only I/O is the same-host PID probe, gated to the local arm):

```rust
pub enum Liveness { AliveLocal, AliveRemote, Stale }

pub fn liveness_for(peer: &Peer, this_host: &str, now_ts: i64) -> Liveness {
    if !is_online_at(peer.last_seen, now_ts) { return Liveness::Stale; }   // recency first
    if peer.host == this_host {
        match peer.pid {
            Some(pid) if !pid_alive(pid) => Liveness::Stale,  // local dead pid ⇒ stale
            _                            => Liveness::AliveLocal, // null pid ⇒ TTL fallback
        }
    } else {
        Liveness::AliveRemote   // remote (incl. empty host): TTL-only, NEVER pid-probed
    }
}
```

- `Liveness::AliveLocal` — same host, within the TTL window, and pid-confirmed
  (or a null-pid TTL fallback, still local).
- `Liveness::AliveRemote` — remote host (incl. empty), within the TTL window,
  liveness presumed by recency only (fail open).
- `Liveness::Stale` — past the TTL window, **or** a same-host row whose known PID
  is dead. `Liveness::token()` returns the stable machine tokens `"alive_local"` /
  `"alive_remote"` / `"stale"`. The pid-confirmed-vs-TTL-presumed nuance is
  surfaced only in the human reason string, not as a fourth variant.

`pid_alive` is a Linux `/proc/<pid>` existence check (no new dependency) and
**degrades to assume-alive** off Linux via `cfg`.

#### `is_alive` delegates (truth table unchanged)

`is_alive(peer) -> bool` is now a thin wrapper —
`!matches!(liveness_for(peer, &this_host(), now()), Liveness::Stale)` — reading the
real `this_host()`/`now()`, so every existing bool call site (`peers`,
`sessions --watch`, `doctor`, the MCP tools) sees **byte-identical** results. The
truth table is unchanged; the enum only adds an observability dimension
(local-vs-remote + reason) on top of the same alive/stale boundary.

#### The liveness reason is surfaced uniformly across all four presence surfaces

`weave scan` (and the `weave_scan` MCP tool) consume `liveness_for` per row to
distinguish remote-host sessions and show *why* a peer is alive — a `<remote>`
marker, a per-row reason string, additive `--json` keys, and a `summary` count
line (see README). Cross-machine liveness inherits the same `ONLINE_TTL_SECS` =
900 s freshness window: a remote peer seen within 15 minutes is presumed alive.

That **same** vocabulary is now surfaced UNIFORMLY across the other three
presence surfaces — `weave peers`, `weave doctor`, and the `sessions --watch`
dashboard (plus the `weave_peers` / `weave_doctor` MCP mirrors) — so all four
read the one classifier and speak one language. This is **display-only**: the
`is_alive` truth table is unchanged (each surface's alive count is still
`!matches!(liveness, Stale)`); no schema, SQL, or `Store`-trait change.

- **`peers`** prints the ` <remote>` marker + `[<reason>]` per row and adds the
  `"liveness"` (token) / `"remote"` (bool) keys to `--json`.
- **`doctor`** computes the three counts in one pass over `views` via
  `liveness_for` and emits a `liveness:` line plus the `--json` keys
  `peers_alive_local` / `peers_alive_remote` / `peers_stale`.
- **`sessions --watch`** classifies each row inside the **pure** render.

Because the dashboard render holds only loose `SessionRow` fields (not a full
`Peer`), `liveness_for` is now a thin wrapper over a field-level seam,
`liveness_from_fields(host, pid, last_seen, this_host, now_ts) -> Liveness`. The
render delegates to it, so the dashboard classifies a `SessionRow`
**deterministically from `(now, this_host)`** with byte-identical results to a
full-`Peer` `liveness_for` call — no behavior change to either path. The render
takes `this_host` (and `now`) as parameters, keeping the pure-render seam intact
(the only env-dependence is the same-host PID probe, gated to the local arm,
exactly as `scan`).

Read paths keep `last_seen` warm: `weave peers` and a long-lived `weave watch`
each refresh presence (heartbeat-on-read, explicit-identity only) so a session
stays visible even with no message traffic.

### Presence dashboard: `weave sessions --watch`

`weave sessions --watch` re-renders a **read-only** presence view of the
federated peers — the same scan model (`federated_peers` joined with `is_alive`),
grouped by `(repo, branch)` — on a fixed interval. The design keeps weave
dependency-light and the loop testable:

- **Pure render seam.** A single pure function
  `render_sessions_dashboard(rows, opts, this_host, now) -> String` does all
  formatting: no I/O, no clock (the `now` is passed in), no sleep. The impure
  watch loop only re-reads a snapshot, calls the pure renderer, prints, and
  sleeps. This mirrors the `commands_for` purity discipline — the renderer is
  unit-testable from hand-built rows against a **fixed `now` + fixed `this_host`**,
  with no store and no terminal. Each `SessionRow` carries `pid` / `last_seen`
  (not a precomputed `alive` bool) so the render classifies liveness itself via
  `liveness_from_fields`, deterministically from `(now, this_host)`.
- **Std-only loop, no new dependency.** The loop is `std::thread::sleep` between
  frames; the in-place redraw is a plain ANSI clear-home literal
  (`\x1b[2J\x1b[H`) gated by `std::io::IsTerminal` on stdout **and** `NO_COLOR` /
  `WEAVE_NO_CLEAR` being unset (otherwise frames are plain, escape-free text). No
  TUI / signal / async crate is introduced — termination is the default SIGINT
  (Ctrl-C), and no raw mode is ever entered, so the terminal cannot be left in a
  bad state. This deliberately mirrors the existing inbox `watch` loop.
- **Read-only.** The loop writes **nothing per tick** — observing presence must
  not perturb it. At most one owner-only self-refresh of the watcher's own row
  runs *once before* the loop (gated on explicit identity, reusing
  `register_peer_full` exactly as `scan` does), never per frame.
- **No store / schema change.** The dashboard consumes already-fetched
  `PeerView` data through the existing backend-agnostic `federated_peers` +
  `is_alive`; there is **no** new `Store` method, no SQL, and no `SessionView`
  change, so both backends are unaffected beyond the shared gate.
- **Bounded iterations for hermetic tests.** `--iterations N` renders exactly `N`
  frames then exits (`0` ⇒ loop forever); the sleep happens *between* frames,
  never after the last, so `--iterations 1` returns immediately. An integration
  test thus drives a single deterministic frame with no hang and no wall-clock
  assertion. The poll `--interval` is clamped in `config` to `[1, 3600]`s
  (`clamp_watch_interval`), reusing the input-cap discipline.

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
- **Session tags are sanitized at the store seam.** The cwd-derived `repo` /
  `branch` / `worktree_id` tags pass through `sanitize_tag` inside
  `register_peer_full` in **both** backends (trim → drop control chars →
  char-boundary-safe `take(MAX_*_LEN)`, each 128) before persistence, so a hostile
  or oversized cwd-derived tag is bounded and control-free, is never re-emitted
  verbatim, and is never an injection target (tags are descriptive only). Capture
  is no-shell argv `git` (§3), so the tag text cannot reach a shell either.
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
  thing cross-store carries is data B chooses to pull and commit itself. This now
  holds **cross-machine**: a remote `libsql`/Turso source (Tier-2 v2, §10) is opened
  read-only too (SELECT-only + write-guard `bail!` + no schema/migrate), so weave
  never writes a remote source. libSQL 0.9.30 has no client-side read-only handle, so
  the recommended deployment contract is a server-enforced read-only Turso token
  (defense-in-depth); weave's own enforcement stands regardless. The remote auth token
  is secret — capped, control-char-rejected, redacted in `Debug`, and never logged,
  injected, or argv'd. The default (sqlite) build refuses remote sources outright with
  a loud stderr note.
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
| Tracked ask/answer/ack | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, pure DB** |
| Ask-many (fan to N peers) | ❌ | ✅ (daemon-mediated) | ✅ **daemon-free, read-time aggregate** |
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
- **weave⊇repowire parity (P1).** repowire's headline advantage over a plain
  mailbox was a **tracked** ask/answer/ack round-trip — but it required the
  long-running daemon to mediate it. weave now closes that gap **daemon-free**: the
  `asks` side-table (§6) layers a correlation-tracked `open → answered → acked`
  lifecycle on the existing append-only `messages` + the caller-side injector, with
  an honest delivery verdict and **no new dependency**. It is local-mesh
  point-to-point in P1; broadcast and cross-store ask are future epics.
- **weave⊇repowire parity (P2 — ask-many).** repowire could also fan one question
  to many peers and collect the replies; weave now matches that **daemon-free** with
  `ask_many` / `ask_many_result` (§6): a small `ask_groups` parent anchor plus the
  additive `asks.parent_id` column turn each target into a normal P1 `ask`, and the
  parent view is computed as a **read-time aggregate** (no background ticker, no stored
  deadline) — `complete | partial | pending` with the totality `answered + acked +
  pending + failed == target_count`. It is **best-effort** (one bad peer is a per-child
  error, not a whole-call failure, matching repowire), **no `store → inject` edge** (the
  per-child nudge is fired caller-side), bounded by `MAX_ASK_MANY_TARGETS = 64`, and
  ships with a **dual-backend additive migration** and **no new dependency**. Explicit
  peer list only (circles compose in a later epic); cross-store fan-out remains future work.

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

### Remote sources — cross-machine pull (Tier-2 v2)

A delivery / federation source need not be a local file. A `StoreSource` (defined in
`config`, below `store`/`main` in the DAG) is either `Local(PathBuf)` or
`Remote { url, token }`, classified by URL scheme (`classify_source`:
`libsql://`/`https://`/`wss://` ⇒ remote, else a local path). Source lists split
**comma-first** (`split_source_list`) so a remote URL is kept whole; the platform
`:`/`;` split still applies only to local fragments. The remote auth token comes from
`pull_token` / `WEAVE_PULL_TOKEN`.

- **Read-only enforcement, now cross-machine.** A remote source is opened with
  `LibsqlStore::open_readonly_remote` (`Builder::new_remote(url, token)`), which sets
  `read_only = true`, runs **no schema/migration/hardening**, and creates no local
  file (a pure `new_remote` connection has no path). The foreign handle is touched
  SELECT-only (`list_peers`/`sessions`/`list_outbox`), every write method hard-traps
  via `guard_writable()` (a `bail!`, not a debug-only assert), and commits land in the
  local owned store with a local per-source cursor advance. The owner-only-writes
  invariant (§7) therefore holds across machines, not just across local files.
- **libSQL 0.9.30 has no client-side read-only handle** — read-only for a pure remote
  connection is a server-side (Turso auth-token scope) property only. The recommended
  deployment contract is a **server-enforced read-only token**
  (`turso db tokens create <db> --read-only`), validated by the server regardless of
  client behavior. weave **cannot mint or introspect** that scope; its own SELECT-only
  + write-guard + commit-local enforcement stands independently as defense-in-depth.
- **Default-backend (sqlite) loud rejection.** The default build has no libsql client,
  so its `store` free functions skip every `Remote` source with a loud stderr note
  (`reject_remote_source`, scheme+host only via `remote_scheme_host`) and count it as
  unsupported (`weave doctor` surfaces `federation_remote_unsupported`). Remote
  sources require a `--features libsql` build.
- **Per-source token resolution.** A source-list entry may carry an inline
  `LABEL=<remote-url>` prefix that selects a distinct token from the env var
  `WEAVE_PULL_TOKEN_<LABEL>`. Resolution is **entirely in `config`** — `StoreSource`
  carries no `label` field (`Remote { url, token, timeout_ms }`): a private
  `parse_labeled_source` splits and validates the label (`is_valid_label`: non-empty,
  ≤ `MAX_LABEL_LEN` = 64, charset `[A-Za-z0-9_]`, uppercased), and only treats the
  prefix as a label when the right side classifies as a remote URL — otherwise the
  whole entry is passed verbatim to `classify_source`. `per_source_token` then resolves
  with precedence **per-source `WEAVE_PULL_TOKEN_<LABEL>` (exact `env::var`, no
  `env::vars()` scan) → shared `WEAVE_PULL_TOKEN` / `pull_token` → none**; the
  per-source value goes through the same `sanitize_token` gate and, if rejected,
  **falls through** to the shared token. The label is a *resolution input only* — it
  is consumed to build the env-var name and never travels on `StoreSource`, into a log,
  or adjacent to a token. An unlabelled (or invalid-label) entry resolves identically
  to before, so the change is backward compatible. The label is not a secret (it names
  the env var); the token is, and must never be inlined. Because `peer_db_sources` and
  `pull_from_sources` both call the SAME `resolve_store_sources`, the LABEL namespace
  (and per-source token) covers remotes in **both** `peer_dbs` and `pull_from` — there
  is one resolver, no second token scheme.
- **Token hygiene.** The token is capped at `MAX_TOKEN_LEN` (8192) with control chars
  rejected (`sanitize_token`), redacted to `<redacted>` by the manual `Debug` on
  `StoreSource::Remote` and on `Config`, and reaches **only** `Builder::new_remote` —
  never a log line, never an argv, never interpolated into SQL or a command string.
  This applies equally to per-source and shared tokens. `weave doctor` re-derives the
  resolved tier per remote source (`PullTokenTier` via `peer_db_remote_token_tiers`, a
  token-free enum) and prints only aggregate counts (per-source / shared / none) on a
  `remote tokens:` line — never a token byte and never a label↔token pairing.
- **Network-failure handling.** libSQL exposes no client timeout knob for a remote
  connection, so each remote `block_on` is bounded by `tokio::time::timeout` (the
  `time` tokio sub-feature, gated behind the existing `libsql` feature — the default
  build gains nothing). A connect/query/timeout error is just another **per-source
  skip** (the existing failure-isolation path: note on stderr, continue), and because
  commits land local-only with a per-intent local cursor advance, the bounded
  single-intent at-least-once / one-intent-per-crash guarantee is preserved unchanged.
- **Per-source remote-call timeout.** The timeout that bounds each remote call is
  resolvable per source on the SAME LABEL namespace as the token, via
  `WEAVE_PULL_TIMEOUT_MS_<LABEL>` (precedence **per-source → global
  `WEAVE_PULL_TIMEOUT_MS` → `REMOTE_TIMEOUT_MS_DEFAULT` (5000 ms)**). It resolves in
  `config` (`per_source_timeout`, mirroring `per_source_token`) — values parsed and
  **clamped to `[MIN_TIMEOUT_MS=50, MAX_TIMEOUT_MS=600000]` ms**; a `0`/unparsable/
  out-of-range value falls through to the next tier (the bound is never disabled). The
  resolved value is carried to the store on the new `StoreSource::Remote.timeout_ms`
  field (NOT a secret; shown verbatim in `Debug`) — it does **not** enter
  `source_cursor_key` (two configs differing only in timeout share one cursor). The
  libSQL backend threads it through `open_readonly_remote(url, token, timeout_ms)` and
  stores it on `LibsqlStore.remote_timeout` so `remote_timeout_for(Option<u64>)` bounds
  both the connect and the read SELECTs; `None` ⇒ the global/default fallback (identical
  to before). `REMOTE_TIMEOUT_MS_DEFAULT` is **owned by `config`** as the single source
  of truth and imported by the store, so the config-resolved and store-fallback paths
  cannot drift. `weave doctor` / `weave_doctor` print a token-free `remote timeout:`
  line (per-source / global / default tier counts via `PullTimeoutTier` +
  `peer_db_remote_timeout_tiers`, plus the effective ms range) — never adjacent to a
  token, never a token byte.

### Per-source token/timeout parity across both source kinds

The two per-source knobs — `WEAVE_PULL_TOKEN_<LABEL>` and
`WEAVE_PULL_TIMEOUT_MS_<LABEL>` — hold at **parity** across **both** federation
source kinds (`peer_db` Tier-1 visibility and `pull_from` Tier-2 delivery) along
three axes:

- **RESOLVED.** Both `peer_db_sources` and `pull_from_sources` route through the SAME
  `resolve_store_sources_with_tiers`, so a labelled remote resolves its token AND
  timeout identically regardless of which list it appears in (one shared LABEL
  namespace, one resolver — no fork).
- **APPLIED.** Every foreign remote open — Tier-1 (`federated_peers` /
  `federated_sessions`) and Tier-2 (`pull_from_store`) — funnels through the single
  `open_source_readonly` → `open_readonly_remote(url, token, timeout_ms)` seam. The
  token reaches `Builder::new_remote`; the timeout bounds both connect and SELECTs.
  There is no source kind that resolves a knob but fails to apply it.
- **SURFACED.** `weave doctor` now reports the resolved tiers/counts for **both**
  kinds. The `peer_db` side already rendered (`federation_remote_*`); the
  previously-missing `pull_from` side is closed by adding the symmetric
  `Config::pull_from_remote_token_tiers` accessor (the sibling of
  `peer_db_remote_token_tiers`) so the rollup treats both kinds uniformly.

The single secret-free rollup is `Config::federation_health() -> FederationHealth`,
holding a `FederationKindHealth` per kind (`peer_db`, `pull_from`) with **only**
counts (`total`/`local`/`remote`, the token tiers, the timeout tiers) and an
effective-ms range (`ms_min`/`ms_max`, `None` over zero remotes so an empty set never
renders a misleading `0-0`) — **never** a token byte nor a label↔token pairing. It is
a **read-only aggregation over already-resolved config tiers** (env/config only),
backend-agnostic, computed via the per-kind `federation_kind_health` helper over the
same `resolve_store_sources_with_tiers` the apply path uses. It adds **no new network
probe**: reachability (ok/skipped) for the `peer_db` set stays the already-computed
`store::federation_status`; the `pull_from` side surfaces resolved counts/tiers only
(opening pull sources for health would be a forbidden new network touch). Both the
CLI `weave doctor` and the `weave_doctor` MCP tool consume this ONE method, so the two
surfaces cannot drift; `main` adds the additive `federation_pull_*` JSON keys + a
`pull sources:` / `pull tokens:` / `pull timeout:` human block, and `mcp` mirrors the
same three human lines.

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

By default the cross-store `from` is advisory **unless a trust set is configured**.
Building with `--features sign` (Ed25519 via `ed25519-dalek`, mirroring the `libsql`
optional-dependency pattern — the **default build links no crypto**) adds verifiable
identity:

- A new low, pure `sign` module (depends only on `config` + std) owns the canonical
  encoding, sign/verify, hex codec, the keypair file, **fingerprints**, and key
  rotation. The private key lives at `~/.config/weave/ed25519.key` (mode `0600`), is
  never logged or printed, and refuses to clobber an existing key.
- The canonical signature covers `(from, to, body)` — **not** `created`/`ts`, which
  is advisory and re-stamped by the receiver on commit, so binding it would be a
  fragile coupling with no integrity gain. Length-prefixed with a
  domain-separation prefix so no field boundary is ambiguous.
- A new `keys(identity, pubkey)` table (always present, plain data, both backends)
  stores peers' public keys. `weave key gen|show|fingerprint|add|list|rotate|revoke`
  (subcommand present only under `--features sign`) manages them.

#### Fingerprints

A **fingerprint** is the SHA-256 of the **raw 32-byte public key**:
`fingerprint_full(pubkey) = hex(SHA256(raw 32 pubkey bytes))` (64 lowercase hex, no
label) is the canonical value trust/revocation match against; `fingerprint(pubkey) =
"SHA256:" + first 16 hex chars` is the **display** form only. Trust and revocation
match on the **full** digest (or a full pubkey hex), so a truncated `SHA256:<16-hex>`
display string never matches — truncation can never cause a mis-trust. The helpers
take the **public** key only and never hash the secret; they return `None` (never
panic) on a malformed/oversized/non-32-byte input.

`sha2` was **already in the `--features sign` dependency tree** (a transitive dep of
`ed25519-dalek`), so declaring it directly under the `sign` feature
(`sha2 = { version = "0.10", optional = true }`, pulled only by `sign`) adds **no new
compiled crate to any graph** — the default and `libsql`-no-sign builds gain nothing.

#### The verification decision table

**Sign on enqueue** (A signs its outbound intent if it has a key); **verify on
commit** in `store::verify_pulled_intent` (B, before its local write), under the
threaded `VerifyPolicy` (tri-state strict override, trust set, revocation list).
B looks up the sender's **registered keys** once via `get_keys` (a lookup error is a
hard drop), then decides. Since #7 the registry is **multi-key** (`identity_keys`): a
signed intent COMMITS IFF the signature verifies against **at least one registered
NON-REVOKED key** for the sender — a revoked key that cryptographically verifies is
*skipped* (R1, absolute revocation), the first non-revoked verifying key is sufficient,
and a signature that verifies against **none** of the registered keys is REJECTED as
before. This is **additive**: with exactly ONE registered key the decision is identical
to the prior single-key model (the table below). The new COMMIT path is legitimate
rotation OVERLAP (old + new key both verify during a window) — something #3's
config-based overlap could only express as trust/strictness, never at the
verification layer (which key may actually verify a message). `is_trusted` /
`is_revoked` match a key's full digest against B's trust/revoked lists;
`trust_configured` = trust set non-empty; an identity is **trusted** if ANY of its
registered keys is in the trust set. The effective strictness for the unsigned /
no-registered-key advisory path is:

```text
if strict_override == Some(true)            => STRICT   (user forced everywhere)
else if strict_override == Some(false)      => ADVISORY (user disabled this path)
else if trust_configured && is_trusted(key) => STRICT   (NEW trust-set default)
else                                        => ADVISORY (current default)
```

Every cell below matches `verify_pulled_intent` exactly (COMMIT = local write,
REJECT = dropped, cursor still advances). Read "the registered key" as "**any
registered non-revoked key**" since #7 — with a single registered key the rows are
unchanged. Two load-bearing rules hold in *every* row: a **present-but-invalid**
signature (verifies against NONE of the registered keys) is ALWAYS rejected, and
**R1** — a signature that verifies ONLY against **revoked** key(s) is rejected
unconditionally (each verifying key's revocation is checked BEFORE acceptance,
before any disable toggle). When a signature verifies against both a revoked and a
non-revoked registered key, the non-revoked match wins (COMMIT) — revocation targets
a specific key, not the identity.

| Sender | Signature | DECISION |
|---|---|---|
| trusted (registered key in trust set) | valid, key not revoked | **COMMIT** (unforgeable, attributed) |
| trusted | present-but-invalid | **REJECT** (always — forgery/tamper) |
| trusted | unsigned | **REJECT** (trusted ⇒ strict-by-default ⇒ must sign) |
| untrusted (trust set configured, sender outside it) | valid | **COMMIT** (advisory — verified, just not pinned) |
| untrusted | unsigned | **COMMIT** (advisory — unsigned operation preserved) |
| any | present-but-invalid | **REJECT** (always) |
| rotation overlap (old + new registered) | valid against either non-revoked key | **COMMIT** (#7 — both keys verify during the window) |
| revoked (verifies ONLY against revoked key(s)) | valid | **REJECT ALWAYS** (R1 — even with strict disabled) |
| no trust set configured | unsigned | **COMMIT** (advisory — UNCHANGED from today) |
| no trust set configured | present-but-invalid | **REJECT** (always) |
| signed but no registered key for sender | present (unverifiable) | advisory path (no fp to trust) ⇒ STRICT only if forced |
| global strict forced (`Some(true)`) | unsigned/unverifiable | **REJECT** (strict everywhere) |
| global strict disabled (`Some(false)`) | unsigned/unverifiable | **COMMIT** (advisory everywhere — but R1 revoked-signed still rejected) |

Two cells went COMMIT→REJECT from the original (pre-Tier-2) model: `trusted+unsigned`
(strict-by-default) and `revoked+valid-sig` (R1). #7 adds exactly one new COMMIT
path — rotation overlap, a sig verifying against a SECOND non-revoked registered key —
and refines R1 to "verifies only against revoked key(s)"; **no row flips
REJECT→COMMIT**, and the single-key model is preserved verbatim. Every no-trust-set
row is byte-for-byte the original behavior; every present-but-invalid row is still
REJECT. Verification reads only B's own `identity_keys` table + B's receiver-local
config; the source is opened read-only (owner-only-writes intact).

#### Rotation & revocation (multi-key registry + receiver-local config)

Trust and revocation lists are **receiver-local config** (no store table); the
**keys** themselves now live in the multi-key `identity_keys` registry (#7). `weave
key add <identity> <pubkey>` **APPENDS** a key (it no longer overwrites), so old + new
coexist for rotation overlap. `weave key rotate` archives the old private key
(`fs::rename` to a `0600` `ed25519.key.<ts>.bak`, never read or printed), generates a
new key, **registers (appends) it without displacing the old one**, and prints **both**
fingerprints plus overlap guidance: keep BOTH keys registered (`weave key add`) so
in-flight messages signed by EITHER key verify during the window, and trust BOTH full
fingerprints in `WEAVE_TRUST`; once peers have the new key, prune the old with `weave
key remove <identity> <old>` and retire it with `weave key revoke <old-full-fp>`.
`weave key remove <identity> <pubkey-or-fingerprint>` deletes one registration (a full
hex pubkey, or a `SHA256:<64-hex>` fingerprint resolved against that identity's
registered set; ambiguous/no match errors). `weave key revoke <fp>` validates the
value and echoes the `WEAVE_REVOKED=` / `revoked = [...]` line to add (it does not
rewrite a managed config); revocation is unconditional (R1). The emitted rotate/revoke
values are the **full** `SHA256:<64-hex>` form so they are actually accepted by
trust/revoke matching. `weave key revoke` additionally writes a best-effort
`declared` row to the `revocations` audit log (provenance only; never a decision
input — see the `revocations` table above). `doctor` reports secret-free per-identity
key counts (`sign_key_identities`, `sign_registered_keys`, `sign_identities_multi_key`),
plus the count of registered keys currently revoked (`sign_registered_keys_revoked`)
and the recorded revocation-event count (`sign_revocation_events`). The MCP
`weave_doctor` tool emits the same sign-gated verify summary at **parity** (strict
mode, trusted/revoked counts, registered-key count, registered-revoked count,
revocation-event count, own fingerprint) — counts + the local fingerprint only,
appended to the JSON-RPC result frame (stdout discipline intact). `weave audit
revocations` lists the log read-only.

`sign` is a low module (`model ← config ← sign`); `store` depends down on it for
verify-on-commit; `main`/`mcp` depend down on both. `VerifyPolicy` lives in `store`
in every build (inert without `sign`) so both backends' free-fn signatures are
identical. No upward edge.
