# weave — Operations & Runbook

This is the operator's guide to installing, wiring, configuring, and
troubleshooting `weave` — the Rust-native agent-to-agent session mesh with a
native multi-mux injector. It complements [README.md](../README.md) (quickstart)
and [ARCHITECTURE.md](../ARCHITECTURE.md) (design); this document is about
*running* weave day to day.

weave is **one Rust binary, no required daemon, no Python**. The core mesh has no
service to start or port to open: the shared SQLite (or libSQL) store is the
broker and the multiplexer CLIs do the pushing. Optional `surfaces` commands are
explicit foreground services: a dashboard listens locally, while Telegram and
Slack bridges keep a poll loop alive. Operating the core is therefore mostly:
install the binary, run `weave setup` once, and occasionally run `weave doctor` /
`weave gc`; run only the optional surface processes you deliberately configure.

---

## 1. Install

### Option A — build from source

```bash
cargo build --release          # -> target/release/weave (sqlite backend, default)
```

The release profile sets `strip = true` and `lto = true`, so the artifact is a
small self-contained binary. Copy it onto your `PATH`:

```bash
install -m 0755 target/release/weave ~/.local/bin/weave
```

### Option B — cargo install

```bash
cargo install --path weave             # from the workspace checkout root
# or, once published:
# cargo install weave
```

This drops `weave` in `~/.cargo/bin`. Make sure that directory is on your
`PATH` (it is by default after a standard rustup install).

### Option C — libSQL build (cross-machine sync)

The `sqlite` and `libsql` backends each statically link their own SQLite C core
and are **mutually exclusive** — a `compile_error!` in `main.rs` rejects
enabling both. To build the libSQL/Turso variant, disable defaults:

```bash
cargo build --release --no-default-features --features libsql
cargo install --path weave --no-default-features --features libsql
```

The `libsql` feature additionally pulls in the libSQL client and a (current-
thread) tokio runtime; the default `sqlite` build pulls in neither. See §6 for
when to choose which.

### Verify the install

```bash
weave --version       # prints the package version AND the backend(s) linked in,
                      # e.g.  0.1.0
                      #       backends: sqlite
```

The `--version` output tells you at a glance whether you are running the
bundled-sqlite build or the libSQL build, without touching a live store.

### Option D — wizard

If you are provisioning a box via the autoinstall/wizard flow, weave ships as
one of the bundled tools. The wizard places the binary on `PATH` and runs
`weave setup` for you; after that, everything below applies unchanged.

---

## 2. `weave setup` — wire into Claude Code

`weave setup` is the one command that makes weave "live" inside Claude Code. It
is **idempotent and safe to re-run**. It does two things:

1. **Registers the MCP server** at user scope via the `claude` CLI:

   ```bash
   claude mcp add weave --scope user -- /abs/path/to/weave mcp
   ```

   Setup first removes any existing `weave` registration, so re-running updates
   the path in place (e.g. after you move the binary from `cargo run` to
   `~/.cargo/bin`). If `claude` is not on `PATH`, setup prints the exact command
   to run later and **continues** — hooks are still installed.

2. **Merges three lifecycle hooks** into `~/.claude/settings.json`:

   | Claude event       | Command              | Effect |
   |--------------------|----------------------|--------|
   | `SessionStart`     | `weave hook session` | Registers this session as an injectable peer (captures its pane). |
   | `UserPromptSubmit` | `weave hook prompt`  | Drains unread messages into the agent's context (and marks them read). |
   | `Stop`             | `weave hook stop`    | Peeks unread (does **not** mark read; see §10). |

Run it:

```bash
weave setup
```

Typical output:

```
weave setup complete:
  exe:      /home/you/.cargo/bin/weave
  settings: /home/you/.claude/settings.json
  MCP:      weave (user scope) -> /home/you/.cargo/bin/weave mcp
  hooks:    added SessionStart, UserPromptSubmit, Stop
```

To reverse everything:

```bash
weave uninstall      # removes the MCP registration + only weave's own hook entries
```

### How the hook merge coexists with rtk / repowire / broker

This box already runs other hook-driven tooling (rtk command rewriting, the
mcp-broker session messenger, and historically repowire). `weave setup` is built
to **never clobber them**:

- **Surgical writes.** Setup reads the existing `settings.json`, adds only its
  three entries, and writes the result. It does **not** template-overwrite the
  file. `serde_json` is built with `preserve_order`, so your existing key order
  is left intact and only weave's additions show up in a diff.
- **Read failures abort.** If `settings.json` exists but cannot be read
  (permissions, EIO), setup **refuses to continue** rather than risk truncating
  it to weave-only hooks. A missing or blank file is treated as an empty object
  (the normal first-run case).
- **Atomic, backed-up writes.** The new settings are serialized to a sibling
  temp file and `rename`d over the target (atomic on POSIX), so a crash or full
  disk mid-write cannot leave you with a truncated settings file. The first time
  weave mutates a pre-existing file it also drops a one-time snapshot at
  `~/.claude/settings.json.weave.bak`.
- **Precise idempotency.** Re-running setup never appends a second weave hook.
  Matching is on the exact installed shape (`<path>/weave hook <session|prompt|
  stop>`, with `weave` as a whole path component) — not a loose substring — so a
  look-alike command like `/usr/bin/myweave hook session` or an rtk/repowire hook
  is never touched or removed. If an old weave hook points at a stale path, setup
  **heals it in place** to the current binary instead of duplicating it.
- **Uninstall is symmetric.** `weave uninstall` removes only entries matching
  that same predicate, prunes now-empty hook arrays, and leaves every rtk /
  repowire / broker / unrelated hook in place.

Because rtk rewrites *commands* (PreToolUse) and weave hooks fire on
*lifecycle* events (`SessionStart` / `UserPromptSubmit` / `Stop`), and the
broker is a separate MCP server, the three compose without contention. weave and
the broker both provide messaging MCP tools; you can run both — weave adds the
native push that the poll-only broker lacks.

> Note: when running weave commands manually in a shell where rtk's hook is
> active, prefix with `rtk` like everything else (`rtk weave doctor`). rtk passes
> weave through unchanged (no dedicated filter), so it is always safe.

---

## 3. CLI surface

`weave <subcommand>`. Run `weave --help` for the full list and the exit-code
contract; `weave man` emits a roff man page.

### Messaging

```bash
weave send --from desktop --to envctl --body "apply the rtk fix"
weave send --to all --body "heads up: rebasing main"     # broadcast (never injected)
weave reply --in-reply-to 42 --from envctl --body "done"  # auto-addressed to sender
weave inbox --me envctl                 # read unread (marks read)
weave inbox --me envctl --peek          # read without marking read
weave inbox --me envctl --all           # include already-read
weave thread --root 42                  # print a conversation by root id
weave receipts --id 42                  # who has read message #42, and when
weave watch --me envctl                 # tail inbox, peeking, until Ctrl-C
```

Operator CLI identity defaults are: **explicit flag
(`--from`/`--me`/`--name`) > config `session` / `$WEAVE_SESSION` > basename of
the current directory.** An operator-invoked `weave inbox` marks read unless
`--peek` is supplied. Automatic lifecycle hooks additionally resolve exact
launcher session-key ownership or one unique same-host client-PID row; only
their unowned basename guess is forced to peek so it cannot consume another
session's inbox. `--to all` (or `*`, `everyone`, `broadcast`) fans out to every
reader; read state is tracked per reader.

Point-to-point recipient resolution also accepts the stable `session_id` shown by
`weave peers --json`, `weave scan --json`, and `weave sessions --json`
(`sess_<16-hex>`). For `send`, `notify`, `ask`, and `job delegate`, Weave resolves
that handle back to exactly one registered peer before writing. Unknown or
ambiguous session ids are rejected instead of silently falling back to a peer
alias, so orchestrators can route by exact live session when human names or mux
targets are ambiguous.

`--subject`, `--body`, and `--text` accept leading hyphens (`allow_hyphen_values`),
so a body like `--body "-n flag broke it"` is taken literally, not parsed as a
flag.

Keyed message writes are byte-stable by design. Supplying an idempotency key to a
`send`, `notify`, `reply`, `ask`, or `answer` stores the caller's body verbatim and
does not auto-prefix current mesh-memory context; an explicit no-memory option has
the same body behavior. This keeps a retry from colliding just because memory
changed between attempts. A `--supersedes` replacement is constrained to one
sender+recipient route: the caller must own the predecessor and the successor must
have the same sender **and recipient**.

### Peers, sessions, presence

```bash
weave register --name desktop    # register this session as an injectable peer
weave peers                      # list peers: presence (online/offline) + injectable?
weave sessions                   # known sessions with unread counts
weave inject --to envctl --text "live nudge"          # test the injector directly
weave inject --to envctl --text "..." --quiet         # inject a content-free ping
```

`register` captures the current pane from the environment (`$TMUX_PANE`,
`$ZELLIJ_SESSION_NAME`, `$WEZTERM_PANE`, `$KITTY_WINDOW_ID`, `$STY`). Presence is
a 900-second heuristic (`is_online` = `last_seen` within 15 min); reading your
own inbox under an explicit identity refreshes your heartbeat.

### Operations

```bash
weave doctor                     # diagnostics (see §9)
weave gc                         # retention sweep (see §7)
weave backup --out mailbox.tar   # no-dep snapshot of DB + config + Claude settings (--force to overwrite)
weave restore --in mailbox.tar   # restore that snapshot (traversal-guarded; --force to clobber DB/config/settings)
weave config init                # scaffold ~/.config/weave/config.toml
weave completions bash           # emit a completion script (bash|zsh|fish)
weave man                        # emit a roff man page
weave mcp --session desktop      # run the MCP stdio server (normally launched by Claude)
weave hook <session|prompt|stop|notification>   # lifecycle hook (reads JSON on stdin)
```

### `--json` for scripting

`inbox`, `thread`, `receipts`, `peers`, `sessions`, and `doctor` all accept
`--json` for machine-readable output. Use it in wrappers, watchers, and CI
instead of parsing the human format. Exit codes are stable:

- `0` — success (**including a failed live injection**: the message is persisted
  and will arrive on the recipient's next drain, so weave still exits 0).
- `1` — runtime error (store/IO failure, unknown backend, missing peer, …).
- `2` — usage error (clap: unknown flag/subcommand, missing argument, bad value).

### Shell completions & man page

```bash
weave completions bash  | sudo tee /etc/bash_completion.d/weave   # system-wide
weave completions zsh   > ~/.zsh/completions/_weave
weave completions fish  > ~/.config/fish/completions/weave.fish
weave man               | gzip -c > ~/.local/share/man/man1/weave.1.gz
```

Both are generated live from the clap command definition, so they always match
the installed binary's actual flags.

### Telegram and Slack bridge operations (`--features surfaces`)

Configure a complete, platform-specific route before starting a bridge. Tokens are
secrets; identities and recipients are weave peer names, and a platform's bridge
identity must differ from its internal recipient.

Authorization is conversation-scoped; there is currently no per-user allowlist.
Every member who can post in the configured chat/channel can invoke read commands,
and `WEAVE_BOT_WRITES=1` grants every such member the bridge identity's write
commands. Use a dedicated private conversation.

```bash
export WEAVE_TELEGRAM_TOKEN='…'
export WEAVE_TELEGRAM_CHAT_ID='…'
export WEAVE_TELEGRAM_IDENTITY='telegram'       # optional distinct default
export WEAVE_TELEGRAM_RECIPIENT='agent'

weave telegram --status --json   # no network, does not consume inbox rows
weave telegram --check --json    # bounded identity + configured-chat access calls
weave telegram                   # blocking poll loop; Ctrl-C stops it
```

Slack uses a bot token and a conversation the app can read:

```bash
export WEAVE_SLACK_TOKEN='xoxb-...'
export WEAVE_SLACK_CHANNEL='C...'
export WEAVE_SLACK_IDENTITY='slack'             # optional distinct default
export WEAVE_SLACK_RECIPIENT='agent'

weave slack --status --json     # no network, does not consume inbox rows
weave slack --check --json      # auth identity + one bounded channel-history read
weave slack                     # blocking poll loop; Ctrl-C stops it
```

Add the Slack app/bot to the configured conversation. Its bot token needs the
matching read scope: `channels:history` for a public channel, `groups:history` for
a private channel, `im:history` for a direct message, or `mpim:history` for a
multi-person direct message. Slack documents these requirements under
[`conversations.history`](https://docs.slack.dev/reference/methods/conversations.history/).
The running relay additionally needs `chat:write` for replies and outbound rows;
see [`chat.postMessage`](https://docs.slack.dev/reference/methods/chat.postmessage).
The Slack check performs `auth.test` followed by one bounded read of the configured
channel. It sends no message and therefore does not validate or claim post access.

Send Slack commands as ordinary history-visible messages: `!weave inbox`,
`!weave peers`, `!weave sessions`, `!weave ask <to> <body>`, and `!weave help`.
Legacy slash-command text remains accepted if it reaches history, but Slack clients
normally reserve leading `/` input for registered slash commands. Telegram keeps
the `/inbox`, `/ask`, and other slash forms.

Do not give both platforms the same bridge identity: simultaneous configuration is
rejected as an identity collision. The legacy `WEAVE_BRIDGE_IDENTITY` is a
compatibility fallback for a single platform, not the preferred dual-platform
configuration.

`weave doctor`, MCP `weave_doctor`, and dashboard settings report
`not_configured`, `not_ready`, `ready_inactive`, `stale`, `degraded`, or `healthy`,
plus the token-free route, runtime-presence/lifecycle, pending count, heartbeat,
and last-success/delivery timestamps. `stale` means a non-stopped owner row exists
but its process heartbeat is outside the shared activity window.
A failed external post leaves its row unread. Delivery is at-least-once; after an
unexpected process stop immediately following remote acceptance, verify the chat
before interpreting a repeated post as a second mesh message.
For Telegram `/inbox`, the accepted snapshot's exact message receipts and update
cursor are one owner-fenced local transaction. Store/ack failure rolls both back;
the remaining duplicate window is only the provider-accepted/local-commit boundary,
because Telegram `sendMessage` has no client idempotency key.

### Cross-store routes and cross-machine push

`--to-store` does **not** name or open the recipient's database. It is a bounded
operator route label that selects cross-store mode; the label is not persisted.
The intent is queued in the sender's own outbox, and the receiver must explicitly
configure that sender store under `pull_from` / `WEAVE_PULL_FROM`:

```bash
# sender A: "build-route" is a label, not a DB selector
weave send --to builder --to-store build-route --to-host buildbox \
  --body "run the release gate"

# receiver B: this is the actual source authorization/address
export WEAVE_PULL_FROM=/path/to/A/messages.db
weave pull --me builder
```

A nonempty `--to-host` is enforced at commit and must exactly match the receiver's
normalized host; omit it for a recipient-name-only route. Pull cursors are scoped
to `(source, recipient, receiver host)`, so one recipient cannot advance past
another's lower outbox id. When upgrading a database with the old source-global
cursor, keyed rows are reconciled automatically. Legacy keyless rows at/below the
old watermark are ambiguous and are skipped with a warning to preserve at-most-once
delivery; this migration posture persists across every bounded page until the route
catches up.

With `--features surfaces`, the sender-initiated equivalent is `weave push`:

```bash
weave push --to builder --host https://buildbox.example \
  --body "run the release gate" --idempotency-key release-2026-07-13
```

The `weave_push` wire action requires an idempotency key. If the CLI flag is
omitted, Weave mints a fresh key for that invocation; two invocations are therefore
two messages. For an uncertain HTTP result, retry with the same explicit key. An
exact replay succeeds without a second row or live nudge; changed semantics under
the key are rejected.

Push accepts only an HTTP(S) origin, optionally ending in `/push`. HTTPS is required
outside `localhost` or an IP loopback address; userinfo, query, fragment, and other
paths are rejected. The bearer token resolves as `--token` then
`WEAVE_PUSH_TOKEN`. Requests time out, responses are capped at 64 KiB before JSON
parsing, and success requires a matching JSON-RPC 2.0/MCP response with the expected
id, result object, boolean `isError`, and text content. Output is bounded/sanitized
and the supplied token is redacted.

The receiver uses a separate push-only credential:

```bash
weave dashboard --token "$WEAVE_ADMIN_TOKEN" --push-token "$WEAVE_PUSH_TOKEN"
# `--write` is optional and controls only the full operator dashboard actions.
```

The push token is accepted only on `POST /push`; it cannot read dashboard state or
invoke another MCP/action tool. It must differ from the operator token. Do not give
remote senders `--token` for the dashboard or general MCP listener.

Weave itself serves plain HTTP. For a non-loopback sender, terminate TLS in a
reverse proxy/private-overlay HTTPS service and forward only `/push` to Weave's
loopback port. A Tailscale/WireGuard IP by itself is not an HTTPS terminator. A
simple alternative is an SSH local forward to the receiver's loopback port, then
use `http://127.0.0.1:<forwarded-port>` from the sending machine.

For `--features sign` deployments, upgrade receivers before senders. New `v2:`
signatures bind `from`, `to`, normalized `to_host`, subject presence/value, body,
stable idempotency key, canonical priority, and TTL; `trace_id` is attempt-local and
excluded. A v2-aware receiver also verifies already queued unprefixed v1 signatures
over their historical `(from, to, body)` tuple, while an old receiver cannot verify
the marked v2 format.

There is currently no receiver switch that retires the unprefixed-v1 compatibility
lane. New send paths emit only `v2:`, but upgraded receivers continue accepting
already-queued v1 rows. Drain or remove legacy queues before treating every accepted
signature as covering priority, TTL, subject, host, and idempotency semantics.

#### Fail-closed signed-v2 migration recovery

On open, Weave refuses the legacy idempotency cleanup if it finds a `v2:` outbox row
whose bound key is missing or collides with an accepted message or an earlier queued
row. The refusal occurs before that row is mutated. Recovery is deliberately offline:

1. Stop every writer and make a byte-for-byte copy/provider snapshot of the store.
2. Inspect the reported outbox id and every row using the same key. For a local
   SQLite or local-libSQL file, use `sqlite3` against the copy; for remote libSQL,
   use the provider's snapshot and SQL console/transaction support.
3. Never edit a `v2:` row's `idempotency_key`, delivery fields, or `sig`
   independently—the signature binds them together. If a safe earlier unsigned/v1
   duplicate can be removed or made keyless, preserve the signed row. Otherwise,
   record the queued semantics, remove the unusable queued row offline, reopen the
   upgraded store, and re-enqueue/re-sign it from the owning sender with a fresh key.
   A `v2:` row whose key is already NULL cannot be reconstructed from the signature.
4. Reopen the store and run `weave doctor`; retain the snapshot until the recreated
   intent has been accepted.

Useful read-only inspection on a copied local store:

```sql
SELECT id, from_peer, to_peer, to_host, subject, body, priority, ttl,
       idempotency_key, sig
  FROM outbox
 WHERE id = <reported-id>;

SELECT 'message' AS source, id, idempotency_key
  FROM messages WHERE idempotency_key = '<reported-key>'
UNION ALL
SELECT 'outbox', id, idempotency_key
  FROM outbox WHERE idempotency_key = '<reported-key>'
 ORDER BY source, id;
```

---

## 4. config.toml

Optional, at `~/.config/weave/config.toml` (honors `$XDG_CONFIG_HOME`). weave
works with no config at all. Scaffold a commented template:

```bash
weave config init        # writes a documented template; NEVER overwrites an existing file
```

The scaffold is created with `0600` permissions in a `0700` directory (it may
hold a libSQL auth token), and `config init` is atomic — a racing writer cannot
cause it to clobber an existing config.

Keys (all optional):

| Key                 | Meaning | Env override |
|---------------------|---------|--------------|
| `session`           | Default identity for this machine/session. **Set this** so presence and read-tracking are reliable; otherwise weave guesses from `basename(cwd)` and won't mark mail read. | `WEAVE_SESSION` |
| `backend`           | `"sqlite"` (default) or `"libsql"`. | `WEAVE_BACKEND` |
| `db`                | Message DB path. Default (sqlite): `~/.local/share/weave/messages.db`. For a *local* libSQL backend this **is** the file path; it is ignored only when a remote `libsql_url` is set. | `WEAVE_DB` |
| `nudge_template`    | Live-injection nudge text. Placeholders `{from}` and `{body}`. Omit `{body}` for a quiet "you have mail" ping with no content. | — |
| `libsql_url`        | Remote libSQL/Turso endpoint (only used with `backend = "libsql"`). | `WEAVE_LIBSQL_URL` |
| `libsql_auth_token` | Auth token for the remote endpoint. **Secret** — redacted from debug output; prefer the env var over storing it on disk. | `WEAVE_LIBSQL_AUTH_TOKEN` |
| `telegram_token` / `telegram_chat_id` | Telegram credential and exact chat scope. | `WEAVE_TELEGRAM_TOKEN` / `WEAVE_TELEGRAM_CHAT_ID` |
| `telegram_identity` / `telegram_recipient` | Distinct weave sender/inbox identity and internal destination. | `WEAVE_TELEGRAM_IDENTITY` / `WEAVE_TELEGRAM_RECIPIENT` |
| `telegram_bot_username` | Optional exact command suffix (for `/command@bot`). | `WEAVE_TELEGRAM_BOT_USERNAME` |
| `slack_token` / `slack_channel` | Slack bot credential and exact readable conversation. | `WEAVE_SLACK_TOKEN` / `WEAVE_SLACK_CHANNEL` |
| `slack_identity` / `slack_recipient` | Distinct weave sender/inbox identity and internal destination. | `WEAVE_SLACK_IDENTITY` / `WEAVE_SLACK_RECIPIENT` |
| `bridge_identity` | Legacy single-platform identity fallback; prefer platform-specific identities. | `WEAVE_BRIDGE_IDENTITY` |

**Environment always wins over the file.** This makes per-session overrides
trivial — e.g. start one agent with `WEAVE_SESSION=envctl` and another with
`WEAVE_SESSION=desktop` from the same config. The resolved `db` and `config`
paths are shown by `weave doctor`.

### Post-send hooks (`[[post_send_hook]]`)

Run an operator-authored external program after a matching send/ack. Hooks are
**config-file-only** — there is **deliberately no env overlay** (a hook is a program
to spawn; injecting one through the environment would be unsafe). Each rule is a
TOML array-of-tables entry:

```toml
[[post_send_hook]]
recipient  = "agent-a"        # "*" = any (the default if omitted/empty); a BROADCAST alias matches a broadcast; else exact
argv       = ["/usr/bin/tee", "/tmp/weave-sentinel"]   # argv[0] resolved to a TRUSTED abs path; no shell, ever
event      = "send"           # "send" (default) | "ack"
timeout_ms = 5000             # clamped to [50, 600000]; omit ⇒ 5000
```

The program is spawned **argv-only (no shell)**; message text is never substituted
into an argv element. Message fields reach the child **only** as environment
variables — the **body is never exported**:

| Env var | Value |
|---|---|
| `WEAVE_HOOK_EVENT` | `send` or `ack` |
| `WEAVE_HOOK_SENDER` | the sender identity |
| `WEAVE_HOOK_RECIPIENT` | the recipient identity (or broadcast alias) |
| `WEAVE_HOOK_SUBJECT` | the message subject |
| `WEAVE_HOOK_MESSAGE_ID` | the message id |
| `WEAVE_HOOK_PAYLOAD` | a small JSON object of the fields above |

Hooks are **fault-isolated and bounded**: a missing/slow/failing hook never breaks
the send (the wait is bounded by `timeout_ms`; failures log to stderr only). The set
is capped at `MAX_POST_SEND_HOOKS`; invalid/oversized rules are dropped at selection.

> **Footgun — hook recursion.** A hook must **not** call back into `weave
> send`/`notify`/`ack` for the same event class, or it re-fires itself in a loop.
> Keep hook programs out-of-band (write a file, post to an external system), not back
> into the mesh. See `docs/SECURITY.md` §3 for the full execution model.

### Backup / restore (`weave backup` / `weave restore`)

`weave backup --out <path>` writes a portable **dependency-free uncompressed USTAR**
archive of a consistent SQLite snapshot (via `VACUUM INTO`, never a raw live-DB copy)
plus `config.toml`, the installed Claude `settings.json` hooks, and a `MANIFEST`; the
archive is read-back-verified before it is declared good. `weave restore --in <path>`
extracts it with a closed traversal allow-list. Both refuse to overwrite without
`--force` (restore takes a `.bak` of `settings.json` first). **Remote libSQL is
unsupported** (there is no local file to snapshot). After a restore, re-run `weave
setup` to re-register the MCP server.

---

## 5. Backends

| Backend  | Default? | Build flag | Sync model | Use it for |
|----------|----------|------------|------------|------------|
| `sqlite` | yes      | (default `--features sqlite`) | synchronous (rusqlite, bundled) | a single local box — the normal case |
| `libsql` | no       | `--no-default-features --features libsql` | async (embedded tokio) | cross-machine sync / Turso replicas |

Both backends share the same schema and the on-disk SQLite format is
**libSQL-compatible**, so the same `messages.db` is portable between builds with
**no migration** — the file is the broker. Both also apply the same hardening on
open: parent dirs created, `busy_timeout=30s`, WAL journal mode,
`synchronous=NORMAL`, idempotent schema, an idempotent `in_reply_to` migration
for pre-threading DBs, and a best-effort `0600` clamp on the DB file so message
bodies are never world/group readable.

Selecting a backend the binary wasn't built with fails **loudly** (a clear error)
rather than silently falling back — so a typo'd `WEAVE_BACKEND` lands nowhere
surprising. A `libsql`-only build (no `sqlite` feature) defaults its backend to
`libsql`.

### When to choose libSQL

Choose `sqlite` (the default) unless you specifically need:

- **Cross-machine messaging** between agents on different hosts, via a shared
  Turso/libSQL endpoint (`libsql_url` + `libsql_auth_token`).
- A **remote replica** of the mailbox.

Caveats for libSQL:
- With a remote `libsql_url` set, the `db`/`WEAVE_DB` path is ignored (weave
  prints a note). For a *local* libSQL file, that path is still the DB.
- WAL/`synchronous` pragmas are only applied to a local libSQL file, not to the
  remote (hrana) path where they don't apply.
- Cross-machine **injection** is explicitly out of scope — only the *mailbox*
  syncs; the live pane push is local to each host. Remote peers receive messages
  via their next hook drain.

If you don't need any of that, stay on bundled `sqlite`: zero extra deps, no
tokio, and the simplest operational footprint.

---

## 6. Storage location & retention (`weave gc`)

The mailbox grows append-only (`messages`, `reads`, `peers` tables). Default
path: `~/.local/share/weave/messages.db` (override via `db` / `WEAVE_DB`; honors
`$XDG_DATA_HOME`).

Prune old messages with `gc`:

```bash
weave gc                              # delete messages older than 30 days (default)
weave gc --older-than-secs 604800     # keep only the last 7 days
weave gc --older-than-secs 0          # delete everything older than "now"
```

`gc` runs in a single immediate transaction: it counts, deletes matching `reads`
rows, then the `messages`, and commits — so receipts never outlive the messages
they refer to. It reports the number of messages deleted. A negative
`--older-than-secs` is clamped to 0.

For a recurring sweep, drive it from cron or a systemd timer (weave has no
internal scheduler — by design, there is no daemon):

```cron
# prune weave mail older than 30 days, nightly
17 3 * * *  /home/you/.cargo/bin/weave gc >/dev/null 2>&1
```

To wipe everything interactively, the MCP `weave_clear` tool with
`scope:"all"` requires an explicit `confirm:true`; the default scope only marks
your own inbox read (non-destructive).

---

## 7. The MCP server

`weave mcp` runs a newline-delimited JSON-RPC 2.0 server over stdio. Claude Code
launches it automatically once it is registered (via `weave setup` or `claude
mcp add`); you rarely run it by hand. It exposes these `weave_*` tools:

`weave_send` · `weave_inbox` · `weave_history` · `weave_sessions` ·
`weave_clear` · `weave_peers` · `weave_reply` · `weave_thread` ·
`weave_receipts` · `weave_doctor` · `weave_whoami`

On `weave_send`, if the recipient is a registered injectable peer, a live nudge
is pushed into its pane; otherwise the message waits and is delivered on the
recipient's next turn. Operational guarantees that matter when something looks
wrong:

- **stdout is protocol-only.** All diagnostics and logging go to **stderr**, so a
  stray log line can never corrupt the JSON-RPC frame stream.
- **No shell, ever.** Every external command (the injector) is spawned with an
  explicit argv vector — never `sh -c` — so message bodies and session names
  cannot be interpreted as shell syntax. SQL is fully parameterized.
- **Bounded inputs.** Identities (`from`/`to`) are capped at 128 chars and
  rejected if empty/whitespace; subjects at 256 chars. Live nudges injected into
  a pane are sanitized (interior control chars collapsed) and truncated to 240
  chars with an ellipsis — the full body still arrives via the store on the next
  drain.

---

## 8. The native injector

The injector pushes a nudge into a *running* recipient's terminal pane by
driving its multiplexer. Detection is by environment variable, probed most- to
least-specific:

| Mux     | CLI binary | Detect env var        |
|---------|------------|-----------------------|
| tmux    | `tmux`     | `TMUX_PANE`           |
| zellij  | `zellij`   | `ZELLIJ_SESSION_NAME` |
| wezterm | `wezterm`  | `WEZTERM_PANE`        |
| kitty   | `kitten`   | `KITTY_WINDOW_ID` (+ `KITTY_LISTEN_ON`) |
| screen  | `screen`   | `STY`                 |

tmux is preferred when both tmux and a terminal are present, because the
multiplexer owns the input line. Submission is **paste-safe** per backend (e.g.
tmux closes bracketed paste before sending Enter) to avoid the mid-tool-call
cancel bug that plagued the older repowire injector.

A failed or impossible injection is **never** fatal: the message is already
persisted before injection is attempted, so the worst case is "delivered on the
next hook drain instead of instantly."

---

## 9. `weave doctor`

The first thing to run when anything looks off:

```bash
weave doctor            # human-readable
weave doctor --json     # machine-readable
```

It reports: weave version, the active **backend**, the resolved **db path** and
**config path**, the **multiplexer detected for the current process** and whether
this session is **injectable** here, total message count, peer count and how many
are online, and whether the `claude` CLI is on `PATH`. Example:

```
weave doctor
  version:        0.1.0
  backend:        sqlite
  db:             /home/you/.local/share/weave/messages.db
  config:         /home/you/.config/weave/config.toml
  this session:   mux=zellij target=my-session injectable=true
  messages:       128
  peers:          3 (2 online)
  claude on PATH: yes
```

---

## 10. Troubleshooting

### "I sent a message but the peer didn't get a live nudge."

This is expected, not a failure, when the recipient is **not running inside a
multiplexer**. Live pane injection requires the recipient to have registered an
injectable target (a known tmux pane / zellij session / etc.). Check:

```bash
weave peers       # is the recipient listed as "injectable"? online?
```

- If the recipient shows `no-inject`, it registered while *not* inside a mux (its
  `$TMUX_PANE`/`$ZELLIJ_SESSION_NAME`/etc. was empty). There is **no live
  injection without a mux** — by design. The message is still persisted and is
  delivered on the recipient's **next turn** via the `UserPromptSubmit` hook
  drain. This graceful-degradation path is the whole point of the hook wiring.
- If the recipient shows `offline`, it simply hasn't been active in the last 15
  minutes; the message still waits in its inbox.
- Confirm your own session is in a mux with `weave doctor` (`injectable=true`).
- Test the injector path directly: `weave inject --to <peer> --text "ping"`. If
  the mux binary is missing from `PATH`, inject reports a clear error naming the
  binary; install the mux or run from inside it.

### "New MCP tools / hook changes aren't taking effect."

Claude Code loads MCP servers and hook config **at session start**. After
`weave setup` (or any change to the registration / `settings.json`), you must
**restart the Claude Code session** for it to pick up the weave MCP server and
the new hooks — they will not load mid-session. Verify wiring first:

```bash
weave doctor                  # claude on PATH: yes ?
claude mcp list               # is "weave" registered?
```

Then start a fresh session.

### "`weave setup` said it couldn't register the MCP server."

It means `claude` was not on `PATH` (or the `claude mcp add` call exited
non-zero). Setup still installed the hooks and printed the exact command to run
later, e.g.:

```bash
claude mcp add weave --scope user -- /abs/path/to/weave mcp
```

Run that once `claude` is available, then restart your session.

### "Messages I see on Stop reappear on the next prompt."

Intended. The `Stop` hook **peeks** (does not mark read) because Claude Code does
not feed Stop-hook stdout into the model on a normal exit — so marking read there
would silently consume messages. The `UserPromptSubmit` drain is the one that
both delivers *and* marks read. The two channels compose: an injectable peer gets
an instant nudge **and** the full message on its next prompt drain.

### "My inbox isn't marking messages read" / "presence shows me offline."

You probably have no explicit identity. weave only marks read and refreshes
presence under an **explicit** identity (a `--me`/`--from` flag, `session` in
config, or `$WEAVE_SESSION`). A name guessed from `basename(cwd)` deliberately
**peeks only**, so it can't consume another session's mail. Fix it by setting
`session` in `config.toml` or exporting `WEAVE_SESSION`.

### "I selected the libsql backend and it errored at startup."

The running binary was not built with the `libsql` feature. Either rebuild with
`--no-default-features --features libsql`, or set `backend = "sqlite"`. weave
fails loudly here on purpose rather than silently using a different store. Check
the linked backend with `weave --version`.

### "Did `weave setup` damage my other hooks?"

It is designed not to, and it leaves you a snapshot. If you ever suspect a
problem, restore from the one-time backup:

```bash
cp ~/.claude/settings.json.weave.bak ~/.claude/settings.json
```

rtk, repowire, broker, and any other unrelated hooks are matched against weave's
exact installed shape and are never touched by `setup` or `uninstall`.

### "The DB file looks lost or I moved machines."

The DB *is* the broker — there's no separate state. Point weave at it with
`WEAVE_DB` / the `db` config key, or copy `~/.local/share/weave/messages.db`. The
on-disk format is libSQL-compatible, so the same file works under either backend
build with no migration. After any move, `weave doctor` confirms the resolved
path and message count.
