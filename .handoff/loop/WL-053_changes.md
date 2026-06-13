# WL-053 — capture tmux server socket in the peer target

## Problem (WL-047 /verify finding)
tmux `Target` carried only the pane id, not which tmux *server* it belongs to, so
inject/spawn/kill relied on the acting process's ambient `$TMUX` and silently hit
`/tmp/tmux-1000/default` (wrong/empty server) for peers on a non-default `-L`/`-S` socket.

## Change (weave-inject/src/inject.rs only — NO schema change)
- `parse_tmux_socket(&str)` (pure) + `tmux_socket_from_env()` — extract the socket PATH
  from `$TMUX` (`<socket>,<pid>,<session>`).
- `tmux_argv(socket, rest)` — build `tmux [-S <socket>] <rest…>`; empty socket = historical argv.
- detect_target (both arms) now sets `Target.socket = tmux_socket_from_env()` for tmux.
- commands_for / kill_commands / liveness_probe tmux arms use `tmux_argv(target.socket, …)`.
- spawn_commands gained a `socket: &str` param; the runner threads this process's `$TMUX`
  socket (a spawn creates the pane in the CALLER's own server).
- Persistence reuses the EXISTING `peers.socket` column (kitty/zellij precedent); registration
  sites already pass `&t.socket` to upsert_peer → end-to-end Target→Peer→Target.

## Tests (weave-inject #[cfg(test)], +3)
- parse_tmux_socket_extracts_path; tmux_argv_inserts_socket_selector;
  tmux_socket_threads_through_all_commands (inject+kill+liveness+spawn pinned; socket-less unchanged).
- All existing tmux argv tests still pass (empty socket ⇒ byte-identical historical argv).

## Gate — GREEN both backends
- fmt clean; clippy -D warnings clean (default + libsql).
- cargo test --all-targets: 569 (was 566). libsql: 529 (was 526).

## Invariants
- No shell (argv vectors only); no SQL touched; no new dep; layer-confined to weave-inject.
- Paste-safe send-keys sequence unchanged (only `-S <socket>` prefixed). Socket is a tmux-provided
  path from trusted env, never user input; pane-id id_valid unchanged.
