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
weave peers                          # list peers + whether each is injectable
weave send --from desktop --to envctl --body "apply the rtk fix"
weave inbox --me envctl              # read (marks read); --peek to not mark; --all to include read
weave inject --to envctl --text "live nudge"   # test the injector directly
weave mcp --session desktop          # run the MCP stdio server
```

Identity resolution: `--from/--me/--name` > `$WEAVE_SESSION` > basename of cwd.
Send `--to all` (or `*`) to broadcast; read state is tracked per-reader.

## MCP tools

`weave_send` · `weave_inbox` · `weave_history` · `weave_sessions` · `weave_clear` · `weave_peers`

On `weave_send`, if the recipient is a registered injectable peer, a live nudge is pushed
into its pane; otherwise the message waits and is delivered on the recipient's next turn.

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

## Status

v0.1.0 — both backends build clean (clippy `-D warnings`), **38 tests green** (22 unit + 16
integration), MCP + CLI + injector + setup automation working; libSQL backend runtime-verified.
Live pane injection is validated by construction (pure command-builder unit tests + fake-mux
integration test); end-to-end mux injection on real tmux/zellij is to be confirmed on the target box.
