#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Runs locally what .github/workflows/ci.yml runs on pull requests and main.
# CI uses the current stable Rust (rust-toolchain with `toolchain: stable`).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
