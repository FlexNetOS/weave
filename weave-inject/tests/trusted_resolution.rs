//! Public trusted-program resolver contract.
//!
//! This lives outside the crate so the import itself proves the spawn/job callers
//! can share one public resolver instead of reimplementing path policy.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use weave_inject::{resolve_trusted, resolve_trusted_program};

fn install_executable(path: &Path) {
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write test executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod test executable");
}

#[test]
fn trusted_resolvers_separate_bare_commands_from_spawn_program_paths() {
    let _lock = weave_core::testenv::lock_env();
    let root = std::env::temp_dir().join(format!(
        "weave-trusted-resolution-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let trusted = root.join("trusted");
    let nested = trusted.join("nested");
    std::fs::create_dir_all(&nested).expect("create trusted test tree");

    let bare_name = format!("cycle_b_runner_{}", std::process::id());
    let trusted_program = trusted.join(&bare_name);
    let nested_program = nested.join("nested-runner");
    let outside_program = root.join("outside-runner");
    install_executable(&trusted_program);
    install_executable(&nested_program);
    install_executable(&outside_program);

    let _mux = weave_core::testenv::EnvVarGuard::set(
        "WEAVE_MUX_DIR",
        trusted.to_str().expect("UTF-8 trusted dir"),
    );

    assert_eq!(
        resolve_trusted(&bare_name),
        Some(trusted_program.clone()),
        "the mux resolver accepts one bare executable name"
    );
    let mux_rejected = vec![
        trusted_program.to_string_lossy().into_owned(),
        "nested/nested-runner".to_string(),
        ".".to_string(),
        "..".to_string(),
        format!("./{bare_name}"),
        format!("nested/../{bare_name}"),
        "../outside-runner".to_string(),
        outside_program.to_string_lossy().into_owned(),
    ];
    for rejected in &mux_rejected {
        assert!(
            resolve_trusted(rejected).is_none(),
            "resolve_trusted must reject path-shaped program input {rejected:?}"
        );
    }

    assert_eq!(
        resolve_trusted_program(&bare_name),
        Some(trusted_program.clone()),
        "the public spawn resolver accepts a bare trusted program"
    );
    assert_eq!(
        resolve_trusted_program(trusted_program.to_str().unwrap()),
        Some(trusted_program.clone()),
        "the public spawn resolver accepts an absolute executable whose canonical parent is trusted"
    );
    let program_rejected = vec![
        "nested/nested-runner".to_string(),
        format!("./{bare_name}"),
        format!("nested/../{bare_name}"),
        "../outside-runner".to_string(),
        nested_program.to_string_lossy().into_owned(),
        outside_program.to_string_lossy().into_owned(),
    ];
    for rejected in &program_rejected {
        assert!(
            resolve_trusted_program(rejected).is_none(),
            "resolve_trusted_program must reject non-bare relative or non-direct-parent absolute input {rejected:?}"
        );
    }

    let _ = std::fs::remove_file(trusted_program);
    let _ = std::fs::remove_file(nested_program);
    let _ = std::fs::remove_file(outside_program);
    let _ = std::fs::remove_dir(nested);
    let _ = std::fs::remove_dir(trusted);
    let _ = std::fs::remove_dir(root);
}
