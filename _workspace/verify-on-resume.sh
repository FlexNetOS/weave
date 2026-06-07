#!/usr/bin/env bash
# verify-on-resume.sh — baseline check a fresh session runs before continuing the loop.
# Used both at RESUME and at the top of every cycle that touches weave's own wire/mux code.
set -euo pipefail
echo "[verify-on-resume] cargo fmt --all -- --check"
cargo fmt --all -- --check
echo "[verify-on-resume] cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings
echo "[verify-on-resume] cargo test"
cargo test --quiet
echo "[verify-on-resume] OK"
