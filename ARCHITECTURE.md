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
├── model.rs         core types + helpers (no I/O)
├── config.rs        config file + env overlay
├── store.rs         Store trait + bundled SQLite backend
├── store_libsql.rs  feature-gated libSQL/Turso backend (cfg(feature="libsql"))
├── inject.rs        native multi-mux injector (pure command tables + runner)
├── mcp.rs           MCP stdio JSON-RPC 2.0 server (weave_* tools)
├── setup.rs         `weave setup` / `weave uninstall` (currently stubs)
└── main.rs          clap CLI; wires config → store → {mcp, cli, hooks}
```

Dependency direction (top depends on bottom):

```
main ──▶ mcp ──▶ store ──▶ model
  │       │        │         ▲
  │       └────────┴── inject ┘   (inject and mcp both use model::Peer)
  └──▶ config        (config feeds main; main feeds store + injector nudge text)
```

### `model.rs` — core types, no I/O

- `Message { id, ts, sender, recipient, subject: Option, body }`
- `Peer { name, mux, target, cwd: Option, last_seen }` — a session that has
  registered itself plus where (if anywhere) it can be injected.
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

`Config { session, backend, db, nudge_template, libsql_url, libsql_auth_token }`,
all `Option`. `Config::load()` reads `~/.config/weave/config.toml`
(`$XDG_CONFIG_HOME` honored) if present, then overlays environment variables:
`WEAVE_SESSION`, `WEAVE_BACKEND`, `WEAVE_DB`, `WEAVE_LIBSQL_URL`,
`WEAVE_LIBSQL_AUTH_TOKEN` (env wins over file). Helpers:

- `backend()` → defaults to `"sqlite"`.
- `db_path()` → config/`WEAVE_DB` override, else
  `$XDG_DATA_HOME/weave/messages.db` (default `~/.local/share/weave/messages.db`).
- `nudge(from, body)` → the live-injection nudge text, from `nudge_template`
  (with `{from}`/`{body}` substituted) or a built-in default that embeds the body.

### `store.rs` — persistence

Owns the `Store` trait (§2), the bundled `SqliteStore`, the SQL schema, the
`ONLINE_TTL_SECS` presence window (900 s), and `is_online(last_seen)`.

### `inject.rs` — native injector

The `Mux` enum, `Target { mux, id }`, environment detection, the pure
per-mux command tables, and the runner. Detailed in §3.

### `mcp.rs` — MCP server

A newline-delimited JSON-RPC 2.0 server over stdio implementing `initialize`,
`ping`, `tools/list`, `tools/call`, and empty `resources/list` / `prompts/list`.
It exposes the six `weave_*` tools and performs the live nudge-inject on send.
stdout is reserved for protocol frames; **all logging goes to stderr**.

### `setup.rs` — Claude Code wiring (stub)

`run(exe)` and `uninstall()` are currently stubs that print a "not yet
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
exact argv, and there are 10 tests total across the crate.

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
| `session` | `SessionStart` | `detect_target()` + `register_peer(name, mux, id, cwd)` — the session becomes an injectable peer. |
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

Three tables, created idempotently:

```sql
messages (id INTEGER PK AUTOINCREMENT, ts INTEGER, sender TEXT, recipient TEXT,
          subject TEXT NULL, body TEXT)
reads    (message_id INTEGER, reader TEXT, ts INTEGER, PRIMARY KEY(message_id, reader))
peers    (name TEXT PRIMARY KEY, mux TEXT, target TEXT, cwd TEXT NULL, last_seen INTEGER)
```

- **`messages`** — the append-only mailbox. `recipient` is a session name or a
  broadcast alias.
- **`reads`** — per-`(message, reader)` read state. This is what makes a
  broadcast deliverable exactly once per reader and keeps each session's "unread"
  independent.
- **`peers`** — the injection registry: where each named session can be reached,
  plus `last_seen` for presence (`is_online` = `last_seen` within
  `ONLINE_TTL_SECS` = 900 s).

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
