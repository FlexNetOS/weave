pub mod inject;

pub use inject::{
    capability, commands_for, commands_for_mode, detect_target, detect_target_with_preference,
    fire_post_send_hooks, have, id_valid, inject as inject_text, inject_mode, kill as kill_target,
    kill_commands, resolve_trusted, resolve_trusted_program, run_post_send_hook,
    spawn as spawn_child, spawn_arg_ok, spawn_commands, target_alive, Capability, Injector, Mux,
    Nudge, SpawnOutcome, Target, MAX_INJECT_CHARS, MAX_SPAWN_ARGS, MAX_SPAWN_ARG_LEN,
};
