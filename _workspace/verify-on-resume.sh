#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
echo "=== verify-on-resume: baseline gate ==="
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo clippy --no-default-features --features libsql --all-targets -- -D warnings
cargo build --no-default-features --features libsql
cargo test --no-default-features --features libsql
echo "=== verify-on-resume: GREEN ==="
