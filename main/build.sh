#!/usr/bin/env bash
# Builds bracket from this checkout and installs it into ~/.cargo/bin.
set -euo pipefail

command -v cargo >/dev/null || { echo "Install Rust first: https://rustup.rs" >&2; exit 1; }
cd "$(dirname "$0")"
cargo install --path . --locked
