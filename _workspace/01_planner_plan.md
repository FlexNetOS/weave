# Plan — WL-012: More mux adapters

## Goal
Verify whether kitty, wezterm, and screen adapters are already implemented and
update the backlog accordingly.

## Investigation
- `weave-inject/src/inject.rs` already contains:
  - `Mux::Kitty` with `commands_for` using `kitten @ send-text`, `--match id:<n>`, `--to <socket>`
  - `Mux::Wezterm` with `commands_for` using `wezterm cli send-text --pane-id --no-paste`
  - `Mux::Screen` with `commands_for` using `screen -S <id> -X stuff`
  - `detect_target()` reads `KITTY_WINDOW_ID` / `KITTY_LISTEN_ON`, `WEZTERM_PANE`, `STY`
  - Liveness probes for all three backends
  - `id_valid` validators for each
  - Unit tests: `kitty_matches_window`, `kitty_honors_listen_socket`, `wezterm_no_paste`, `screen_stuffs_cr`, etc.

## Conclusion
WL-012 is a **duplicate** of already-shipped work. No code changes required.

## Changes
- `backlog.md` — flip WL-012 to `- [x]` with a duplicate note.
- `TASKS.md` — flip the mux adapters line to `- [x]`.
