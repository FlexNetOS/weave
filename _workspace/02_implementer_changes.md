# Implementer changes — WL-013

## What changed
1. **`weave-core/src/config.rs`**
   - Added `pub mux_preference: Option<String>` to `Config` struct.
   - Added `pub fn mux_preference(&self) -> Option<&str>` accessor.
   - Updated config template doc-comment to show `mux_preference = "zellij"`.

2. **`weave-inject/src/inject.rs`**
   - Added `detect_target_with_preference(preferred: Option<Mux>) -> Target`.
   - `detect_target()` is now a thin wrapper calling `detect_target_with_preference(None)`.
   - When `preferred` is `Some(mux)`, only that mux's env var is checked.
   - Added unit tests:
     - `detect_target_with_preference_honors_kitty_over_tmux`
     - `detect_target_with_preference_returns_none_when_missing`

3. **`weave-inject/src/lib.rs`**
   - Exported `detect_target_with_preference`.

4. **`weave/src/main.rs`**
   - Added `parse_mux_preference(cfg: &Config) -> Option<Mux>` helper.
   - Changed `RealInjector` from unit struct to hold `preferred_mux: Option<Mux>`.
   - Updated all 7 `detect_target` call sites to pass the parsed preference.

## Build confirmation
- `cargo build` — green
- `cargo build --no-default-features --features libsql` — green
