pub mod inject;

pub use inject::{
    capability, commands_for, commands_for_mode, detect_target, have, id_valid,
    inject as inject_text, inject_mode, resolve_trusted, target_alive, Capability, Injector, Mux,
    Nudge, Target, MAX_INJECT_CHARS,
};
