# weave

**Rust-native agent-to-agent session mesh with a native injector.**

Let coding-agent sessions (Claude Code, etc.) message each other — and **push into a
running session's terminal pane** (tmux *or* zellij) so a peer is flagged the moment a
message arrives. One static binary. No Python, no daemon, no external dependency on
repowire.

See [.handoff/context/PRD.md](.handoff/context/PRD.md) for the full design and [.handoff/loop/TASKS.md](.handoff/loop/TASKS.md) for the roadmap.

## Why

Claude Code sessions are isolated. Prior local tools were either **poll-only** (no push)
or **tmux-only + Python** (no native zellij injector). weave is a single Rust binary that
pushes natively into tmux and zellij, and degrades to hook-delivery-on-next-turn when no
multiplexer is present.

## Build

```bash
cargo build --release      # -> target/release/weave
```

## Use with your coding agent

`weave setup --provider <claude|codex|gemini|aider>` wires weave into your host's
own config file (default `claude`). Every provider write is **idempotent**
(re-running is a no-op), **never clobbers foreign content** (only weave's own
entry is touched), **atomic** (temp + rename, with a one-time `.weave.bak`
snapshot at `0o600`), and **read-back verified** (re-read + re-parse + assert
weave's entry landed and every foreign entry survived before reporting success —
a non-NotFound read error aborts without writing). Reverse with
`weave uninstall --provider <…>`.

| Provider | Target file | What weave writes | Mechanism |
|---|---|---|---|
| `claude` (default) | `~/.claude/settings.json` | Registers the `weave` MCP server (via `claude mcp add`) + merges four `hooks.{event}` entries (`SessionStart`→`session`, `UserPromptSubmit`→`prompt`, `Stop`/`SubagentStop`→`wake`). | **Confirmed.** |
| `codex` | `~/.codex/config.toml` | Sets the top-level `notify` argv key: `notify = ["<weave>", "hook", "wake"]`. | **Partially confirmed** — Codex's documented automation hook is `notify`; it has no per-event granularity, so weave maps it to its drain (`hook wake`). Written Rust-natively (not via the ecc `.codex` sidecar). |
| `gemini` | `~/.gemini/settings.json` | Merges the same Claude-shaped `hooks.{event}` block. | ⚠ **UNCONFIRMED** — Gemini CLI uses a Claude-style JSON settings file, but its exact lifecycle-hook key is not confirmed. weave scaffolds the documented best-known (Claude-compatible) shape; update if Gemini confirms a different key. |
| `aider` | `~/.aider.conf.yml` | Appends a minimal `weave-hook:` stanza (hand-templated YAML, no YAML dependency). | ⚠ **LIMITED** — Aider has no rich lifecycle-hook surface; this is a best-effort scaffold and may be ignored until Aider grows hook support. |

### CC Switch provider bridge

If you already manage model providers in CC Switch, `weave provider-switch` can
apply those providers without launching the Tauri app:

```bash
weave provider-switch list --app claude
weave provider-switch current --app codex
weave provider-switch switch --app codex deepseek
```

The bridge is available in the default sqlite build and reads `~/.cc-switch/cc-switch.db` (override with `--db`), supports
`claude`, `codex`, and `gemini`, updates CC Switch's current-provider marker, and
writes the corresponding live host config. Unlike a plain settings overwrite, it
preserves existing weave lifecycle wiring where present: Claude `hooks`/
`mcpServers`, Codex `notify = ["…", "hook", "wake"]`, and Gemini settings
keys outside the provider's own config block. Use `--dry-run` on `switch` to
validate a provider id without writing.

The Claude flow registers the MCP server (per user, all projects) and wires the
lifecycle hooks so sessions auto-register and auto-receive. The resulting
`~/.claude/settings.json` looks like:

```jsonc
{
  "hooks": {
    "SessionStart":      [{ "hooks": [{ "type": "command", "command": "weave hook session" }] }],
    "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": "weave hook prompt" }] }],
    "Stop":              [{ "hooks": [{ "type": "command", "command": "weave hook wake" }] }],
    "SubagentStop":      [{ "hooks": [{ "type": "command", "command": "weave hook wake" }] }]
  }
}
```

(You can also register the MCP server by hand: `claude mcp add weave --scope user -- /path/to/weave mcp`.)

Now any session can use the `weave_*` MCP tools, and `weave hook prompt` surfaces unread
messages into the agent's context on its next turn (auto-delivery without a multiplexer).

> The `codex`/`gemini`/`aider` mechanisms marked above are scaffolded with the
> caveat noted; weave writes the closest documented config entry Rust-natively
> and prints the caveat on each run. Track the gaps in
> [`docs/MULTI-SURFACE-PARITY.md`](docs/MULTI-SURFACE-PARITY.md).

## CLI

```bash
weave register --name desktop        # register this session (captures pane from $TMUX_PANE/$ZELLIJ_SESSION_NAME)
weave attach --name desktop          # adopt a *running* session into the store WITHOUT restarting (re-capture pane)
weave peers                          # list peers + host-aware liveness reason + remote marker + injectable + tags
weave scan                           # refresh your own git tags, then list every (federated) peer + liveness + tags
weave scan --repo weave --branch feat/x --json   # filter by exact repo/branch tag; machine-readable output
weave sessions --watch               # live read-only presence dashboard, grouped by repo then branch (Ctrl-C to stop)
weave sessions --watch --interval 5 --repo weave  # re-render every 5s, narrowed to one repo
weave connect --to envctl            # probe whether a peer can be live-nudged right now (verdict only)
weave send --from desktop --to envctl --body "apply the rtk fix"
weave send --to envctl --body "use the v2 schema instead" --supersedes 41   # REPLACE msg #41 (hidden from unread, flagged in history); supersede=replacement, reply=threading
weave send --to envctl --body "transient status" --ttl 3600   # ephemeral: auto-deleted after N seconds (delete-on-sweep), excluded from every read surface; capped at 24h
weave inbox --me envctl              # read (marks read); --peek to not mark; --all to include read
weave export --out mailbox.html      # self-contained, offline, searchable HTML of your mailbox (--for <id> to scope; --limit N to cap)
weave backup --out mailbox.tar       # no-dep snapshot of DB + config + Claude settings (--force to overwrite)
weave restore --in mailbox.tar       # restore that snapshot (traversal-guarded; --force to clobber DB/config/settings; remote libSQL unsupported)
weave session export --out s.json    # portable JSON of a session (messages + mesh memory) for cross-instance resume (--for <id>; --force)
weave session import --in s.json     # import that session into THIS instance (idempotent; --as <id> to remap; --dry-run for counts)

# Tracked ask/answer/ack (correlation-tracked request/response — distinct from send/reply)
weave ask --from desktop --to envctl --body "can you confirm the schema?" --subject "schema"
weave answer --id ask_42_17 --body "confirmed, ship it" --from envctl   # or: --in-reply-to <msg_id>
weave ack --id ask_42_17 --from envctl --message "thanks"               # close the thread (acked)
weave asks --me envctl --role askee  # list tracked asks (role: asker|askee|any); --json
weave ask-get --id ask_42_17         # inspect one ask (state + answer presence); --json

# Ask-many: fan ONE question to N peers, collect replies (best-effort, non-blocking)
weave ask-many --from desktop --to envctl --to ci --body "ready to ship?" --subject "release"
weave ask-many-result --parent-id askm_7_31   # aggregate: per-child state + pending list + complete|partial|pending; --json
weave ask-many-result --parent-id askm_7_31 --age 600   # treat still-open children past 600s as partial

# Job board (poll-only, daemon-free durable work queue)
weave job create --title "build the release" --desc "cut v0.2" --assignee ci --kind build
weave job list --state queued --owner desktop --limit 20   # filter by state/owner/creator/assignee/circle; --json
weave job show <job_id>              # full row (status is the canonical; show is an alias)
weave job status <job_id>            # same as show
weave job claim <job_id> --as ci     # mint a fresh attempt_id (the fencing token) + assign; prints it
weave job update <job_id> --attempt <att_id> --state running --note "compiling"  # fenced by attempt_id
weave job update <job_id> --attempt <att_id> --state completed --result '{"ok":true}'  # JSON result/error/artifacts
weave job result <job_id>            # terminal payload (summary/result/error/artifacts) or not_ready
weave job cancel <job_id> --reason "superseded"   # cooperative cancel request (worker honors it)

# Circles + orchestrator role (P4): visibility scoping + a per-circle coordinator
weave peers --circle team-a          # scope a listing to one circle (default: your own circle)
weave peers --all-circles            # mesh-wide (circle='*'); also on `sessions`/`scan`
weave orchestrator claim             # claim the single coordinator slot for your circle
weave orchestrator claim --force     # steal it from a live holder (non-destructive role flip)
weave orchestrator status --circle team-a   # who (if anyone) is the live orchestrator

# Rich presence (P5): self-reported turn_state + free-form description
weave describe "reviewing PR #23"    # set a short self-description (TTL'd, control-stripped, ≤200 chars)
weave status working                 # explicitly set your turn_state (pending_first_turn|working|awaiting_input|idle)
                                     # (turn_state is normally auto-set by the lifecycle hooks — see below)

# Presence daemon (optional heartbeat for live liveness)
weave daemon start                   # start the background heartbeat daemon (idempotent)
weave daemon stop                    # stop the daemon
weave daemon status                  # show whether the daemon is running and its PID
# The daemon writes heartbeats every 15s and evicts stale rows every 60s.
# When stopped, liveness degrades transparently to the TTL heuristic.

# Notify + delivery observability (P6): fire-and-forget + transport-side trace
weave notify --from desktop --to envctl --body "heads up"   # no reply expected; prints the honest delivery verdict
weave notify --to envctl --body "still waiting?" --dedup-idle   # idle ping: supersedes YOUR prior unread idle pings to envctl (collapse to latest); never touches a real message or another sender's pings
weave delivery --id 42               # show the transport trace (queued -> injected/not_injectable -> drained); --json
                                     # (the complement to `weave receipts`, which shows READ receipts)

weave inject --to envctl --text "live nudge"   # test the injector directly

# Spawn / kill a peer (argv-only, no shell) — launch an agent into a mux pane/window, or kill its pane/session
weave spawn worker --cmd claude --cmd --dangerously-skip-permissions   # spawn `worker` into a new pane (mints + threads a birth cert)
weave spawn worker --cmd claude --window --cwd /path/to/repo --mux tmux  # new window instead of split; explicit cwd + mux override
weave kill worker                    # kill the peer's pane (tmux/kitty/wezterm) or session (zellij/screen, coarse)

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
**repo / branch / worktree** tags. It now also **distinguishes remote-host
sessions** and shows a per-row liveness *reason* (pid-confirmed-local vs
TTL-presumed-remote vs stale). `--repo <name>` / `--branch <name>` narrow the set
by exact tag match.

`--json` emits an array of
`{name, repo, branch, worktree, mux, pane, host, alive, liveness, remote, origin, foreign}`.
The two new keys are additive:

- **`liveness`** — a stable machine token, one of `"alive_local"` (same host,
  online, pid-confirmed or null-pid TTL fallback), `"alive_remote"` (a different
  host, online by TTL only — never pid-probed), or `"stale"` (past the TTL window,
  or a same-host known-dead pid).
- **`remote`** — a bool, `true` when the row's `host` differs from this machine's
  host.

The human (non-`--json`) output marks a remote row with a ` <remote>` tag and
prints the reason in brackets per row:

```
<name>[ <remote>] [<reason>] repo=… branch=… worktree=… mux=… pane=… host=…[ (via <store>)]
```

where `<reason>` is one of `alive (local, pid)` (same host, PID confirmed),
`alive (local, ttl)` (same host, null PID, presumed alive by TTL),
`alive (remote, ttl)` (another host, presumed alive by TTL), or `stale`. When at
least one row is listed, a trailing summary line counts the regimes:

```
summary: N local-alive, M remote-alive, K stale
```

**Cross-machine liveness is TTL-only.** A remote-host peer is presumed alive when
its `last_seen` is within the presence TTL (`ONLINE_TTL_SECS`, 900 s) — it is
**never pid-probed across hosts** (weave can only probe a process on the machine it
runs on; see ARCHITECTURE "A2 — fail-open by host"). The same TTL window is
inherited cross-machine, so a peer seen within 15 minutes reads online.

The same repo/branch/worktree tags also appear in `weave peers`, `weave sessions`
(via a local-only display join), and the `weave doctor` `peers_tagged` count.

**One liveness language across `scan` / `peers` / `doctor` / `sessions --watch`.**
The exact same host-aware vocabulary `scan` uses — the machine tokens
`"alive_local"` / `"alive_remote"` / `"stale"`, the human reasons
`alive (local, pid)` / `alive (local, ttl)` / `alive (remote, ttl)` / `stale`,
the ` <remote>` marker, and the `N local-alive, M remote-alive, K stale`
breakdown — is now surfaced **uniformly** on the other three presence surfaces:

- **`weave peers`** marks a remote row with ` <remote>` and prints its liveness
  reason in brackets (between the `[online|offline]` presence token and the
  existing `[target]` token); `--json` gains the two additive keys
  `"liveness"` (the stable token) and `"remote"` (bool, `host != this_host`).
- **`weave doctor`** prints a `liveness:` line —
  `N local-alive, M remote-alive, K stale` — and `--json` gains the three sibling
  counts `"peers_alive_local"` / `"peers_alive_remote"` / `"peers_stale"`
  (alongside the existing `"peers"` / `"peers_online"` / `"peers_tagged"`).
- **`weave sessions --watch`** shows the per-row reason marker + ` <remote>` on
  each dashboard row and the three-count breakdown in its header (see below).

This is display-only — the `is_alive` truth table is unchanged; every surface
reads the same pure classifier (`store::liveness_for`), so all four speak one
consistent liveness language.

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
`weave sessions [<ts>] — N session(s), A local-alive, B remote-alive, C stale, R repo(s), D branch(es)`
(with any active `repo=…`/`branch=…` filters echoed) — i.e. the **same**
three-count liveness breakdown `weave scan` / `weave doctor` print, followed by
one section per `(repo, branch)` group in sorted order. Each section header reads
`[<repo> / <branch>] G session(s), GA alive` (an empty tag renders as `-`), and
each row is `  <name>[ <remote>] [<reason>] worktree=… mux=… host=…`, plus
` (via <store>)` for a federated peer from another store. The per-row `<reason>`
is the same vocabulary as `scan` — `alive (local, pid)` / `alive (local, ttl)` /
`alive (remote, ttl)` / `stale` — and a remote-host row carries the ` <remote>`
marker. A group exceeding the per-section row budget (20) prints the first 20
rows then a `  +N more` line. An empty snapshot renders the zeroed header plus
`no sessions`.

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
· `weave_ask` · `weave_answer` · `weave_ack` · `weave_asks` · `weave_ask_get`
· `weave_ask_many` · `weave_ask_many_result`
· `weave_job_create` · `weave_job_list` · `weave_job_show` · `weave_job_status` · `weave_job_claim`
· `weave_job_update` · `weave_job_result` · `weave_job_cancel`
· `weave_claim_orchestrator` · `weave_orchestrator_status` (P4 circles)
· `weave_set_turn_state` · `weave_set_description` (P5 rich presence)
· `weave_notify` · `weave_delivery` (P6 notify + delivery observability)
· `weave_spawn_peer` · `weave_kill_peer` (WL-047 agent spawn/kill — DANGEROUS, off in safe HTTP mode)

`weave_spawn_peer` `{ name, cmd:[…argv], cwd?, mux?, window?, circle? }` launches a
new agent into a fresh mux pane (or `window:true`) **argv-only — no shell, ever**,
mints a birth certificate in the parent and threads identity (`WEAVE_SESSION`) +
cert (`WEAVE_BIRTH_CERT`) into the child's env so it self-registers an unguessable
identity on its first `weave hook session`. The child program (`cmd[0]`) is
constrained to weave's trusted-dir set, and the cwd is gated by the spawn allowlist
(`spawn_allowed_dirs` / `WEAVE_SPAWN_DIRS`): the MCP tool **denies by default** when
no allowlist is configured. `weave_kill_peer` `{ name }` looks up the registered
peer's `(mux, target)` and issues the per-mux kill argv. Both are in
`DANGEROUS_TOOLS`, so they are disabled on the safe HTTP surface unless `--dangerous`.

`weave_peers` / `weave_sessions` / `weave_scan` take an optional `circle` arg
(`"*"` = mesh-wide; omitted = your circle), and `weave_whoami` echoes your circle +
role (and now your `turn_state` + `description`).

`weave_set_turn_state` `{ state }` and `weave_set_description` `{ description }` are
**owner-only** self-setters (bound to the caller's identity). turn_state is normally
auto-set by the lifecycle hooks; the description is free-form, capped (oversized
truncates, never errors), and TTL'd. `weave_peers` / `weave_scan` surface both
compactly (non-idle turn_state + a live description only), `weave_whoami` always.

On `weave_send`, if the recipient is a registered injectable peer, a live nudge is pushed
into its pane; otherwise the message waits and is delivered on the recipient's next turn.

`weave_notify` `{ to, body, subject? }` is a **fire-and-forget** (no-reply) notification: it
persists a normal message, fires the same live nudge if the recipient is injectable, and
returns the **honest delivery verdict** — `transport_delivered` (nudge landed live) /
`queued_next_turn` (registered/not-alive — arrives next drain) / `recipient_not_injectable`.
An unknown peer is honest success (the message waits), not an error; it is point-to-point
(use `weave_send` for broadcast) and opens no tracked thread (the difference from `weave_ask`).
`weave_delivery` `{ message_id }` shows the **transport** trace (queued → injected /
inject_failed / not_injectable → drained) — the complement to `weave_receipts` (which shows
READ receipts). The trace is **metadata-only** (it never carries the message body); an
unknown/never-traced id returns an empty trace, not an error.

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

## Tracked ask/answer/ack

`weave_send`/`weave_reply` are **fire-and-forget**: they deliver a message and forget
it. `weave_ask`/`weave_answer`/`weave_ack` add a **correlation-tracked request/response
thread** on top of the same mailbox + injector — use them when you need to *track*
whether a request was answered and close the loop.

- **`weave ask` / `weave_ask`** `{ from?, to, body, subject?, reply_to? }` — opens a
  tracked thread to a peer and **returns a `correlation_id` immediately** (non-blocking —
  *not* a synchronous RPC, *not* a delivery receipt). The state starts `open`. Pass
  `reply_to` (a prior correlation_id) to chain a follow-up: the prior thread is acked and
  the new question links into the same conversation (`weave thread` renders the chain).
  Point-to-point only — a broadcast `to` is rejected (use `weave_send` for broadcast).
- **`weave answer` / `weave_answer`** `{ from?, correlation_id? | in_reply_to?, body }` —
  replies along the correlation chain **back to the original asker** (state → `answered`).
  Accepts either the `correlation_id` or an `in_reply_to` message id that resolves to the
  owning ask.
- **`weave ack` / `weave_ack`** `{ from?, correlation_id, message? }` — closes/acknowledges
  the thread (state → `acked`). A pure state transition; an optional closing `message` is
  recorded as a note (not delivered/nudged in this version).
- **`weave asks` / `weave_asks`** `{ me?, role? }` (role `asker|askee|any`, default `any`)
  and **`weave ask-get` / `weave_ask_get`** `{ id }` — list / inspect tracked asks
  (read-only).

The lifecycle is **monotonic** — `open → answered → acked`, never backward — so a
double-ack, an answer to an already-acked thread, or an unknown correlation_id is a clean
error, never a silent regression.

The question and answer text reuse the ordinary `messages` table (threaded via
`in_reply_to`); a small `asks` side-table holds the correlation_id + lifecycle state, so
the live nudge, hook drain, `weave thread`, and receipts all work for ask/answer with no
new machinery. It is **daemon-free** (a synchronous DB write + the existing caller-side
nudge) and local-mesh only — cross-store ask is future work.

**Honest delivery verdict.** Each `ask`/`answer` fires the same caller-side live nudge as
`weave_send` and reports an honest verdict derived from the existing injector return —
`transport_delivered` (a nudge actually reached the pane), `queued_next_turn` (registered
but not live; delivered on the recipient's next drain), or `recipient_not_injectable` (no
injectable pane). The verdict is **advisory, never an error**: a queued or not-injectable
ask still succeeds and arrives on the next drain.

## Ask-many (fan one question to N peers)

`ask` is point-to-point; `ask_many` fans **one question to an explicit list of peers** and
lets you collect their replies — still **daemon-free** and built directly on the `asks`
table (each child is a normal `ask`, answered/acked exactly like P1). No quorum, no retry,
no background ticker — best-effort, just like repowire's `ask_many`.

- **`weave ask-many` / `weave_ask_many`** `{ from?, to:[peer,…], body, subject? }` — opens a
  parent ask-many (`askm_<id>`), creates **one child `ask` per peer**, fires each child's
  live nudge caller-side, and **returns immediately** with the `parent_id`, every child's
  `correlation_id`, and every child's honest delivery verdict (`transport_delivered` /
  `queued_next_turn` / `recipient_not_injectable`). **Best-effort per child:** an
  unknown/unreachable/broadcast peer in the list yields a per-child error but does **not**
  fail the whole call. `to` is an **explicit peer list** (circles compose in a later epic),
  capped at **64** targets (an empty or over-cap list is a hard error); the list is de-duped.
- **`weave ask-many-result` / `weave_ask_many_result`** `{ parent_id, age? }` — **read-only**
  aggregate of the children at read time: each child's state (`open`/`answered`/`acked`),
  the answers collected, the still-pending peers, the rollup counts, and a
  `complete | partial | pending` summary. `complete` once no child is pending; `partial`
  only when you pass `age` and an open child has been waiting at least that many seconds
  (there is no stored deadline); otherwise `pending`. Totality always holds:
  `answered + acked + pending + failed == target_count`.

Children answer and ack through the **unchanged** `weave answer` / `weave ack` path, so
`weave thread`, receipts, and the hook drain all work for an ask-many child with no new
machinery. Local-mesh only (each child must be an explicit, valid, non-broadcast peer in
your own store); cross-store fan-out is future work.

## Job board (poll-only, daemon-free)

A **durable work queue** on top of the same store — the third step toward
repowire capability parity. A job is a persistent row with a lifecycle; workers
**poll and claim** jobs and report progress/results back. It is **poll-only**:
there is **no autonomous dispatch or agent-spawn** in this release (a worker is
whatever process polls the board — a runner that *acquires and runs* a job by
spawning an agent is deferred to a later epic). Still **daemon-free**, **no new
dependency**, and **local-mesh** only.

**The board model.**

- **Create** a durable job (`weave job create` / `weave_job_create`) — it starts
  `queued`, the server mints its `job_<…>` id, and the creator becomes the owner.
  Carries title/description/kind plus optional assignee/owner/circle and
  caller-supplied `deadline_at`/`expires_at`.
- **List / show / status** (`weave job list|show|status`) are read-only. `list`
  filters by state/owner/creator/assignee/circle and is bounded; `show` and
  `status` are aliases for the same single-job view.
- **Claim** (`weave job claim` / `weave_job_claim`) is how a worker takes a job:
  it **mints a fresh `attempt_id`** (the fencing token), assigns the job to the
  worker, and moves it to `running`. The worker captures the printed `attempt_id`.
- **Update** (`weave job update` / `weave_job_update`) drives the lifecycle
  forward — state, phase, an append-only progress note, and the terminal
  `result` / `error` / `artifacts` (all **TEXT JSON**). Once a job is claimed,
  an update **must carry the matching `attempt_id`** or it is rejected
  (`stale_attempt`); an unclaimed job accepts a tokenless update (pre-claim
  parking).
- **Result** (`weave job result` / `weave_job_result`) returns the terminal
  payload (summary/result/error/artifacts) once the job is in a terminal state,
  otherwise a `not_ready` marker.
- **Cancel** (`weave job cancel` / `weave_job_cancel`) is **cooperative, never a
  hard delete**: a still-`queued` job transitions straight to terminal
  `cancelled`; an in-flight (claimed/running) job only gets a `cancel_requested`
  flag set, which the worker observes on its next poll and honors. No daemon is
  needed to *request* a cancel — the worker does the honoring.

**`attempt_id` fencing.** The claim→token→update fencing is enforced **in the
store**, so the CLI and the MCP tools inherit it identically. Re-claiming an
in-flight job mints a **new** token that fences out the prior worker: any update
carrying the now-stale token is rejected. A worker that wants to retry a failed
job creates a new one (terminal states — `completed` / `failed` / `cancelled` /
`expired` / `unavailable` — are frozen).

The job board adds only an additive `jobs` table (both backends, guarded
idempotent migration — a legacy DB upgrades in place) and routes every CLI and
MCP path through one set of store methods. There is **no `store → inject` edge**
(jobs don't nudge in this release) and the autonomous JobRunner / scheduler /
spawn machinery is explicitly out of scope here.

## Circles + orchestrator role

A **circle** is a visibility-scoping group on a peer (a `peers.circle` column,
default `"default"`). `weave peers` / `sessions` / `scan` default to the **caller's
circle**; pass `--circle <name>` to scope to another, or `--all-circles` (MCP:
`circle='*'`) to go **mesh-wide**. An **orchestrator** caller defaults to mesh-wide
visibility (the repowire rule, daemon-free). Set your circle with the `circle`
config key or `WEAVE_CIRCLE` (resolved like `WEAVE_SESSION` resolves identity). With
everyone in `"default"` and no flag, every listing is **byte-identical** to a
pre-circles weave — a single-circle deployment is unchanged.

The **orchestrator role** (a `peers.role` column; an enum `peer | orchestrator`,
never free text) is the single per-circle coordinator:

- `weave orchestrator claim [--circle <c>] [--force]` (`weave_claim_orchestrator`)
  promotes the caller. It is **claimed, never self-asserted** — a fresh registration
  is always `role='peer'`, and a **re-register PRESERVES** an existing orchestrator
  (it can never silently demote you). A claim while a **different LIVE** orchestrator
  holds the circle is **refused** unless `--force` **steals** it: in ONE transaction
  every other orchestrator in the circle is demoted to `peer` and the caller is set.
  The forced steal is a **non-destructive role-bit flip** (the demoted peer can
  re-claim; no data is lost), so it is **not** confirm-gated.
- `weave orchestrator status [--circle <c>]` (`weave_orchestrator_status`) reports
  the live holder (or that none is present). **"Live" REUSES `is_alive`** — the same
  daemon-free liveness verdict the peer listings use (no new probe, no heartbeat).
- `weave_whoami` (MCP) echoes your resolved circle + role.

Circles and roles are **pure DB**: two additive `peers` columns (both backends,
guarded idempotent migration — a legacy DB upgrades in place reading
`circle='default'`/`role='peer'`), no new dependency, and **no `store → inject`
edge** (a circle/role never reaches the injector). The forced demote is the only
cross-row peer write P4 adds — a single-row UPDATE in the caller's **own** store
(never a foreign store); a peer still can never set another peer's circle/identity.

## Rich presence (turn_state + description)

Two self-reported presence fields on each peer — the fifth repowire-parity epic,
still **daemon-free**, **no new dependency**, **local-mesh**, and **owner-only**:

- **`turn_state`** — a lifecycle signal, one of `pending_first_turn | working |
  awaiting_input | idle` (unset reads `unknown`). It is **auto-set by the lifecycle
  hooks** — zero-friction, no explicit call needed:

  | Hook event | Sets turn_state |
  |---|---|
  | `session` (SessionStart) | `pending_first_turn` (registered, no turn yet) |
  | `prompt` (UserPromptSubmit) | `working` (mid-turn) |
  | `stop` (Stop) | `idle` (turn finished) |
  | `notification` (Notification) | `awaiting_input` (agent prompt live + unconsumed) |

  The hook write is **best-effort** and runs *after* the message drain/registration,
  so a turn_state update failure can never sink delivery. An explicit setter is also
  available — `weave status <state>` / `weave_set_turn_state` (enum-validated; a
  non-enum value is rejected) — but the hooks make it optional.

- **`description`** — a free-form self-set string (`weave describe <text>` /
  `weave_set_description`), capped at **200 chars** and control-stripped. It carries a
  **900 s read-time TTL** (the same `ONLINE_TTL_SECS` liveness window, on its own
  `description_ts` so it ages out independently of liveness): a description older than
  the window simply reads blank, computed at read time with **no sweeper** and without
  mutating the stored row. Set early in a session to advertise what you're working on.

Both are **owner-only** — every setter binds the row to the caller's own resolved
identity (`UPDATE … WHERE name = me`); a peer can never set another peer's presence.

**Compact, non-noisy display.** `weave peers` / `weave sessions` / `weave scan` add a
marker **only** when there's something to show — a `[working]` / `[awaiting-input]` /
`[pending]` tag (idle/unknown adds nothing) and a short `"…"` description suffix (an
unset or expired description adds nothing) — so an unset peer's output is byte-identical
to a pre-P5 weave. `weave_whoami` always shows the `turn_state` and `description` lines
(`-` when unset — whoami is a verbose self-report). `--json` only **adds** the
`turn_state` / `description` / `description_ts` keys.

Rich presence is **pure DB**: three additive `peers` columns (both backends, guarded
idempotent migration — a legacy DB upgrades in place reading `unknown`/empty), and
**no `store → inject` edge** (presence never reaches the injector).

## Native injector

| Mux | Detect (env) | Inject |
|-----|--------------|--------|
| tmux | `TMUX_PANE` | `tmux send-keys -t <pane> -l <text>` + `Enter` |
| zellij | `ZELLIJ_SESSION_NAME` | `zellij --session <name> action write-chars <text>` + `write 13` |

`commands_for()` is a pure, unit-tested function; `inject()` checks the mux is on PATH and
falls back cleanly (caller uses next-turn delivery) if the pane/session is gone.

### Spawn / kill (WL-047)

The same per-mux command tables back `weave spawn` / `weave kill` and
`weave_spawn_peer` / `weave_kill_peer`: `spawn_commands()` / `kill_commands()` are
pure, exact-argv functions (no shell, every positional after an end-of-options
`--` where the CLI supports it). Spawn captures the new pane/window id where the mux
echoes one; muxes that don't are **fail-open** and lean on the child's own
self-registration. Kill is exact where the mux can address a pane, **coarse**
(whole-session teardown) where it can't.

| Mux | Spawn (pane default; `--window` → window) | Kill | Notes |
|-----|-------------------------------------------|------|-------|
| tmux | `tmux split-window -P -F '#{pane_id}' -c <cwd> -- <argv…>` (window: `new-window`) | `tmux kill-pane -t <id>` | id echoed (`%n`) → peer pre-registered |
| zellij | `zellij action new-pane -- <argv…>` (window: `new-tab`) | `zellij delete-session --force <name>` | **fail-open** (no id echoed); kill is **coarse** (whole session) |
| kitty | `kitten @ launch --type tab --cwd <cwd> --env WEAVE_SESSION=… -- <argv…>` (window: `--type os-window`) | `kitten @ close-window --match id:<id>` | id echoed → peer pre-registered |
| wezterm | `wezterm cli spawn --cwd <cwd> -- <argv…>` (window: `--new-window`) | `wezterm cli kill-pane --pane-id <id>` | id echoed → peer pre-registered |
| screen | `screen -dmS <name> <argv…>` | `screen -S <name> -X quit` | **fail-open** (no id echoed); kill is **coarse** (whole session) |
| iterm2 / none | — (not supported, fail-open) | — | spawn/kill report "unsupported on this mux" |

Spawn is gated by two layers: the child program (`argv[0]`) must resolve inside
weave's trusted directories, and the cwd must fall under the spawn allowlist
(`spawn_allowed_dirs` / `WEAVE_SPAWN_DIRS`) — deny-by-default for the MCP/remote
surface, warn-but-proceed for the operator-local CLI.

## Human surfaces (`--features surfaces`)

Optional **human** surfaces, all Rust-native (no Next.js, no Python, no async
runtime) and behind one feature flag. The **default build links none of this** —
the bots reuse the same optional `reqwest` client `--features llm` already carries,
so enabling `surfaces` adds **one** shared copy, and the default `cargo build` adds
zero. These are **CLI subcommands, not MCP tools**, so the MCP surface is unchanged.

```bash
cargo build --release --features surfaces        # composes: --features "libsql surfaces"

# Read-only web dashboard (sessions/presence, recent messages, jobs, leases,
# schedules). Localhost-bound, bearer-gated; a random token is printed to stderr
# when --token is omitted. Open http://127.0.0.1:8788/ with that bearer token.
weave dashboard                                   # default port 8788
weave dashboard --port 9000 --token mysecret

# Telegram / Slack bridges (poll-only): relay between a chat and the mesh.
WEAVE_TELEGRAM_TOKEN=… WEAVE_TELEGRAM_CHAT_ID=… weave telegram
WEAVE_SLACK_TOKEN=… WEAVE_SLACK_CHANNEL=…        weave slack
```

The dashboard is **read-only** (`GET /` HTML, `GET /events` SSE — never mutates) and
HTML-escapes every stored string. Bot tokens are **secrets** — supply them via
config (`telegram_token` / `slack_token`) or the env vars above (envctl can inject
them); they are Debug-redacted and never logged. The bridge posts inbound human
replies into the mesh as the configured `bridge_identity` (`WEAVE_BRIDGE_IDENTITY`,
default `telegram`/`slack`). See ADR-0004 for the locked stack decision.

## Governed web access (`--features obscura`)

weave is the **governance plane** for stealth web access — it does **not** link a
browser. Behind a default-OFF `--features obscura`, weave spawns the separate
[`obscura`](https://github.com/FlexNetOS/obscura) browser binary (`obscura mcp`) as
a child via **argv-only `std::process::Command` (never a shell)** and speaks
newline-delimited JSON-RPC over its stdio as a hand-rolled MCP **client**. No
V8/tokio/obscura crate is linked — the default `cargo build` adds **zero** deps
(std + the already-present `serde_json` carry the whole client). obscura is a
*runtime* dependency (a separate installed binary), not a compile-time one.

```bash
cargo build --release --features obscura          # composes: --features "libsql obscura"

# Deny-by-default: configure an allow-list first (config.toml or env), then drive a
# browser op. ONE op per call; all 35 obscura browser_* ops are reachable.
WEAVE_OBSCURA_ALLOW_OPS=navigate,snapshot,extract \
  weave web navigate --url https://example.com
weave web --list                                  # enumerate the ops (no spawn)
weave web --stop                                  # reap the cached obscura child
```

The agent-facing MCP surface grows by **one** token-light tool, not 35:
`weave_web {action, args, describe?}` (ADR-0003 — per-op schemas fetched on demand
via `describe`). Every web op is **deny-by-default**: refused unless the operator
explicitly allows it (`obscura_allow_ops` / `WEAVE_OBSCURA_ALLOW_OPS`, or `"*"`),
and every URL is **SSRF-guarded** — loopback / `localhost` / link-local
(`169.254.*` incl. the cloud-metadata endpoint) / RFC1918 private / `*.local` /
bare-IP targets are blocked unless `obscura_allow_internal=true`. Optional
`obscura_allow_domains` narrows to a domain allow-list (or `"*"` for any public host;
SSRF still blocks internal); `--lease-ttl` rate-limits per
host; `--audit` records a durable job. Web access reuses weave's existing
permission/lease/job primitives (the same gate as any other mesh work) and
`weave_web` is gated as a **dangerous** tool (blocked in safe HTTP mode). The
obscura child's stderr and any proxy/token secrets are never logged. See ADR-0002.

## Post-send hooks (`[[post_send_hook]]`)

Run an operator-authored external program after a matching send/ack — useful for an
out-of-band notification, a metric, or a sentinel write. Hooks are **config-file-only**
(deliberately **no env overlay** — a hook is a program to spawn, so injecting one via the
environment would be unsafe) and live in `config.toml`:

```toml
[[post_send_hook]]
recipient  = "agent-a"        # "*" = any (the default if omitted/empty); a BROADCAST alias matches a broadcast; else exact
argv       = ["/usr/bin/tee", "/tmp/weave-sentinel"]   # argv[0] resolved to a TRUSTED abs path; no shell, ever
event      = "send"           # "send" (default) | "ack"
timeout_ms = 5000             # clamped to [50, 600000]; omit ⇒ 5000
```

The spawn is **argv-only — no shell, ever.** weave never substitutes message text into an
argv element; `argv[0]` must resolve inside weave's trusted directories. Message fields
reach the child **only** as environment variables (`Command::envs`), never as argv:

| Env var | Value |
|---|---|
| `WEAVE_HOOK_EVENT` | `send` or `ack` |
| `WEAVE_HOOK_SENDER` | the sender identity |
| `WEAVE_HOOK_RECIPIENT` | the recipient identity (or broadcast alias) |
| `WEAVE_HOOK_SUBJECT` | the message subject |
| `WEAVE_HOOK_MESSAGE_ID` | the message id |
| `WEAVE_HOOK_PAYLOAD` | a small JSON object of the fields above |

The **message body is NOT exported** (no leak into the child's `environ` / `ps e`). Hooks
are **fault-isolated and bounded**: a missing/slow/failing hook never breaks the send — the
wait is bounded by `timeout_ms` and every failure is logged to stderr only, never
propagated and never on the MCP JSON-RPC stdout frame. There is **no new standing MCP
tool**; hooks fire from `weave send`/`notify`/`ack` and `weave_send`/`weave_notify`/
`weave_ack` via one shared helper.

> **Footgun — hook recursion.** A hook must **not** call back into `weave send` (or
> `notify`/`ack`) for the same event class, or it will re-fire itself in a loop. Keep
> hook programs out-of-band (write a file, post to an external system), not back into
> the mesh.

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

#### `weave doctor` federation-health rollup — both source kinds, symmetric

`weave doctor` reports a **federation-health rollup** that now covers **both**
federation source kinds — `peer_db` (read-only visibility) **and** `pull_from`
(cross-store delivery) — symmetrically. For each kind it summarizes:

- **source counts** — total resolved, split into local-file vs remote-URL;
- **per-source token tiers** — how each remote resolved its token (per-source /
  shared / none);
- **per-source timeout tiers** — how each remote resolved its call timeout
  (per-source / global / default) plus the effective ms range over the remotes.

The `peer_db` side renders on the existing `remote sources:` / `remote tokens:` /
`remote timeout:` lines (JSON keys `federation_remote_*`). The `pull_from` side
renders on `pull sources:` / `pull tokens:` / `pull timeout:` lines, emitted **only
when `pull_from` is configured** so a local-only config is byte-unchanged. The new
`--json` keys (counts/tiers only) are:

- `federation_pull_sources`, `federation_pull_local`, `federation_pull_remote`
- `federation_pull_token_{per_source,shared,none}`
- `federation_pull_timeout_{per_source,global,default}`
- `federation_pull_timeout_ms_{min,max}` (only when a remote pull source exists, so
  an all-local set never renders a misleading `0-0`)

The rollup is **secret-free** — it carries only tier *counts* and an ms range, never
a token byte nor a label↔token pairing — and is mirrored in the `weave_doctor` MCP
tool. It completes per-source token/timeout **parity** across both federation source
kinds: the same knobs were already resolved and applied for `pull_from`, and
`doctor` now surfaces them at the same level as `peer_db`. The pull-side rollup is a
read-only view of the already-resolved config — it adds **no** network probe (no
reachability is shown for `pull_from`).

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
weave key add envctl <hex-pubkey>   # APPEND a peer's public key (multi-key: old + new coexist for rotation overlap)
weave key remove envctl <key-or-fp> # prune a retired key (full hex pubkey OR a SHA256:<64-hex> fingerprint)
weave key list                      # list ALL registered (identity, public key, fingerprint) keys, with [trusted]/[REVOKED] tags (--json)
weave key rotate --me desktop       # archive the old key (0600), generate a new one, KEEP the old registered during overlap, print both fingerprints
weave key revoke <fingerprint>      # print the value to add to WEAVE_REVOKED to retire a key (config-driven; also logs a `declared` audit event)
weave audit revocations             # list the observed-revocation audit log (declared + enforced events), secret-free (--json, --limit)
```

The private key lives at `~/.config/weave/ed25519.key` (mode `0600`) and is never
logged or printed. A signed intent makes the cross-store `from` **unforgeable**; a
**tampered or spoofed signature is always rejected** regardless of mode.

Keys live in a **multi-key registry** (`identity_keys`): each identity may have
**several** registered keys at once. A signed pulled intent commits IFF its signature
verifies against **at least one registered non-revoked key** for the sender. `weave
key add` **appends** (it no longer overwrites — re-adding the same key is a no-op);
`weave key list` shows every key per identity (with its fingerprint and a `[trusted]`
or `[REVOKED]` tag); `weave key remove` prunes one; and `weave doctor` reports
secret-free per-identity key counts. Up to `16` distinct keys per identity are allowed.

#### Rotation overlap (zero-drop key change)

Because the registry is multi-key, an old and a new key can both verify during a
rotation window — no in-flight message signed by the old key is dropped:

1. On the rotating session: `weave key rotate --me desktop` generates the new key and
   keeps the old one registered locally; it prints both fingerprints.
2. On each receiver: `weave key add desktop <new-pubkey>` (the old key stays
   registered alongside it), and trust BOTH full fingerprints in `WEAVE_TRUST`.
   Messages signed by EITHER key now verify and commit.
3. Once every peer has the new key, retire the old one:
   `weave key remove desktop <old-pubkey-or-fp>` to drop it from the registry and
   `weave key revoke <old-full-fp>` to revoke it (R1 — a signature against a revoked
   key is rejected unconditionally even if it cryptographically verifies).

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

#### Revocation audit log (`weave audit revocations`)

The R1 revocation predicate above stays **absolute and config-driven** — the
decision reads only `WEAVE_REVOKED` / `revoked`, never the store. Alongside it, a
sign-gated **observed-revocation audit log** (the additive `revocations` table,
present as inert plain data in every build) records *when revocation is exercised*,
purely for operator visibility — it is **never read by the verifier** and so can
never weaken or diverge from R1:

- A **`declared`** event is recorded when an operator runs `weave key revoke <fp>`
  (provenance: which fingerprint was marked revoked, and when).
- An **`enforced`** event is recorded (best-effort) at the moment the absolute R1
  predicate rejects a pulled signed intent because its signature verified only
  against a revoked key. An audit-write failure is logged to stderr and swallowed —
  it can never change the rejection decision.

`weave audit revocations` lists the log most-recent-first (`--json`, `--limit`).
The log is **secret-free**: it stores and prints fingerprints (`SHA256:<64-hex>`),
public identities, source labels, and counts only — never a private key, peer
pubkey, or token.

Both `weave doctor` and the MCP `weave_doctor` tool emit a sign-gated
**verify-summary** at parity: strict-verify mode, the trusted set / revoked set
counts, the registered-key count, the number of registered keys currently revoked,
the recorded revocation-event count, and this session's own fingerprint — counts
and the local fingerprint only, never a peer key.

## Codex 7-layer harness

The checked-in autonomous weave-loop can be driven through the binary:

```bash
weave harness ide-merge-ide            # dry-run: print the seven layers + exact env
weave harness ide-merge-ide --execute  # run the loop (ralph-weave.sh)
```

Dry-run is the default and prints the seven layers plus the exact `WEAVE_*`
environment handed to the runner. It wraps
`.claude/skills/weave-loop/scripts/ralph-weave.sh`: Kimi Code plans/reviews,
Ollama launches Claude MiniMax for the implementation pass, and durable
`.handoff/loop` sentinels control resume/handoff. `--json` emits the plan
machine-readably; `--safe` keeps destructive applies disabled inside the loop.
The script is spawned argv-only (`bash <script>`), never via a shell string.

## Status

v0.2.0 — a small internal Cargo workspace (`weave-core` ← `weave-inject` ← `weave-mcp` ← `weave`)
that links to **one** dependency-light static binary. Both backends build clean under clippy
`-D warnings` and `cargo fmt --check`; **706 tests green** on the default `sqlite` backend and
**657 green** on `--features libsql` (the `sign` and `surfaces` feature combos are CI-gated too).
MCP + CLI + native multi-mux injector + `weave setup` automation are all working.

The MCP surface is **token-light** (ADR-0003): ~70 `weave_*` operations are exposed through a
single standing `weave` meta-tool via progressive disclosure, not a flat per-op table, with a
CI-enforced standing-token budget. Full **CLI parity** is the zero-standing-cost path.

Live pane injection is **validated end-to-end on real tmux and zellij** (including a live
RTX-5090 box) — `connect` → Live, `notify`/`send` → `injected/ok` delivery trace — on top of the
pure command-builder unit tests and fake-mux integration tests.

Optional, default-OFF features add **zero** compiled deps to the default build: `libsql`
(libSQL/Turso backend), `sign` (ed25519 message signing), `llm` (thread summarization),
`surfaces` (read-only web dashboard + Telegram/Slack bridges), and `obscura` (governed,
deny-by-default web access).

## See also

- **[Stalwart](https://github.com/stalwartlabs/stalwart)** — A Rust-native, secure mail &
  collaboration server (JMAP, IMAP, SMTP, CalDAV, CardDAV, WebDAV). Stalwart's pluggable
  storage abstraction, JMAP protocol design, and observability stack are prior art for
  any future weave external-API work. Note: Stalwart is AGPL-3.0; weave does not embed
  its code and references it only as architectural inspiration.

- **[FrankenNetworkX](https://github.com/Dicklesworthstone/franken_networkx)** — A
  memory-safe, deterministic Rust reimplementation of NetworkX with a Canonical Graph
  Semantics Engine (CGSE). weave embeds three `fnx-*` crates via Cargo git dependencies
  (`fnx-classes`, `fnx-algorithms`, `fnx-runtime`) to power the `weave graph` command:
  peer/message network analysis with connected components, degree centrality, and density.
