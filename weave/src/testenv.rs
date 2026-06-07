//! Test-only process-env serialization (compiled only under `#[cfg(test)]`).
//!
//! This bin-crate wrapper re-exports the canonical implementation from
//! `weave-core` so unit tests in the binary crate share the same global lock
//! and RAII guards.

#[allow(unused_imports)]
pub use weave_core::testenv::*;
