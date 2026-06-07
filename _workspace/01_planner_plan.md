# Plan — WL-013: Config file mux preference

## Goal
Add `mux_preference` to `~/.config/weave/config.toml` so users can override the
auto-detection order when multiple multiplexer env vars are present.

## Current state
- Config file already supports `session` (default identity) and `nudge_template`.
- `detect_target()` auto-detects mux from env vars in hardcoded priority:
  tmux → zellij → wezterm → kitty → screen.
- No user override exists.

## Changes

### 1. Config model (`weave-core/src/config.rs`)
- Add `pub mux_preference: Option<String>` to `Config` struct.
- Add `pub fn mux_preference(&self) -> Option<Mux>` that parses the string via
  `Mux::parse` (case-insensitive, accepts `"tmux"`, `"zellij"`, etc.).
- Update the config template doc-comment to mention the new key.

### 2. Injector (`weave-inject/src/inject.rs`)
- Change `detect_target()` signature to accept `preferred: Option<Mux>`.
- If `preferred` is `Some(mux)`, check ONLY that mux's env var and return the
  corresponding target (or `Target::none()` if the env var is absent).
- If `preferred` is `None`, preserve today's auto-detection order exactly.

### 3. CLI / hooks (`weave/src/main.rs`)
- Make `RealInjector` a struct holding `preferred_mux: Option<Mux>` instead of
  a unit struct.
- Update the single `&RealInjector` construction site to pass
  `cfg.mux_preference()`.
- Update all 6 direct `inject::detect_target()` call sites to pass
  `cfg.mux_preference()`.

### 4. Tests
- Unit test in `inject.rs`: `detect_target_with_preference_honors_kitty_only`
  sets `KITTY_WINDOW_ID` and `TMUX_PANE`, passes `Some(Mux::Kitty)`, asserts
  kitty is returned; pass `Some(Mux::Tmux)` asserts tmux is returned.
- Unit test in `config.rs`: `mux_preference_parses_valid_and_rejects_invalid`
  checks accepted strings and that garbage returns `None`.

## Invariants
- No shell (only env var reads, no subprocesses in `detect_target`).
- No Store changes (config-only).
- No new default dependency.
- `detect_target(None)` is byte-identical to today's behavior.

## Risks
- Changing `detect_target()` signature touches 7 call sites in `main.rs`; a
  missed site would fail to compile (caught by `cargo build`).
