//! Test-only process-env serialization (compiled only under `#[cfg(test)]`).
//!
//! Process environment variables are *global, mutable* state. `std::env::set_var`
//! / `remove_var` mutate that shared table, and Cargo runs unit tests on multiple
//! threads, so any two tests that touch the same (or an overlapping) `WEAVE_*` var
//! can interleave and read each other's writes — a genuine cross-thread data race,
//! not merely a flaky assertion.
//!
//! The rule this module enforces is simple and crate-wide: **every** unit test that
//! reads or writes a `WEAVE_*` (or any other process-global) env var MUST hold
//! [`lock_env`] for its entire body, and mutate the env via [`EnvVarGuard`] so the
//! prior state is restored on scope exit — even on panic. This is the single
//! canonical guard; modules reach it as `crate::testenv` from their own
//! `#[cfg(test)]` blocks. It lives in no non-test build (`#[cfg(test)]`), so it
//! never enters the shippable binary and adds no dependency.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The ONE process-wide lock every `WEAVE_*`-touching unit test serializes on.
fn env_mutex() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire the global env lock for the current test, returning a guard that holds
/// it until dropped.
///
/// Poison-tolerant: if a previous holder panicked (poisoning the mutex), we recover
/// the guard via `into_inner()` rather than propagating the poison. A single
/// panicking test must not cascade-fail or deadlock the rest of the suite — the
/// panicking test still fails on its own merits; this only keeps the *lock* usable
/// so unrelated tests run and report their own results.
pub fn lock_env() -> MutexGuard<'static, ()> {
    env_mutex().lock().unwrap_or_else(|p| p.into_inner())
}

/// RAII set/restore for one environment variable.
///
/// On construction it records the variable's prior value; on `Drop` it restores
/// exactly that prior state — re-setting the old value, or *removing* the var if it
/// was absent before. Restore runs even if the test panics, preventing `WEAVE_*`
/// leakage into later tests.
///
/// SAFETY / correctness: callers MUST already hold [`lock_env`] for this guard's
/// whole lifetime. The lock is what makes the process-global `set_var`/`remove_var`
/// it performs race-free; this type only guarantees restoration, not exclusion.
pub struct EnvVarGuard {
    key: String,
    prev: Option<OsString>,
}

impl EnvVarGuard {
    /// Set `key=val`, remembering the prior value for restoration on `Drop`.
    pub fn set(key: &str, val: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, val);
        Self {
            key: key.to_string(),
            prev,
        }
    }

    /// Remove `key`, remembering the prior value for restoration on `Drop`.
    pub fn remove(key: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EnvVarGuard::set` overrides while in scope and restores the prior value on
    /// drop.
    #[test]
    fn env_var_guard_restores_prior() {
        let _g = lock_env();
        let key = "WEAVE_TESTENV_RESTORE_PRIOR";
        // Establish a known prior value outside the guarded scope.
        std::env::set_var(key, "original");
        {
            let _v = EnvVarGuard::set(key, "override");
            assert_eq!(std::env::var(key).as_deref(), Ok("override"));
        }
        assert_eq!(
            std::env::var(key).as_deref(),
            Ok("original"),
            "prior value must be restored on drop"
        );
        std::env::remove_var(key);
    }

    /// When the var was absent before, the guard removes it again on drop.
    #[test]
    fn env_var_guard_removes_when_absent() {
        let _g = lock_env();
        let key = "WEAVE_TESTENV_ABSENT";
        std::env::remove_var(key);
        {
            let _v = EnvVarGuard::set(key, "transient");
            assert_eq!(std::env::var(key).as_deref(), Ok("transient"));
        }
        assert!(
            std::env::var_os(key).is_none(),
            "absent var must be removed again on drop"
        );
    }

    /// `EnvVarGuard::remove` clears the var in scope and restores the prior value.
    #[test]
    fn env_var_guard_remove_restores_prior() {
        let _g = lock_env();
        let key = "WEAVE_TESTENV_REMOVE_RESTORE";
        std::env::set_var(key, "keep-me");
        {
            let _v = EnvVarGuard::remove(key);
            assert!(std::env::var_os(key).is_none());
        }
        assert_eq!(std::env::var(key).as_deref(), Ok("keep-me"));
        std::env::remove_var(key);
    }

    /// `lock_env` is poison-tolerant: a prior panic that poisoned the mutex must
    /// not prevent a later acquisition from succeeding.
    #[test]
    fn lock_env_recovers_from_poison() {
        // Poison the canonical mutex from a child thread that panics while holding it.
        let _ = std::thread::spawn(|| {
            let _g = lock_env();
            panic!("intentional poison");
        })
        .join();
        // The next acquisition must still succeed (recovered, not propagated).
        let _g = lock_env();
    }
}
