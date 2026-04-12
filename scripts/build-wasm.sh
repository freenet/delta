#!/bin/bash
# Reproducible WASM build for site-contract and site-delegate.
#
# Produces byte-identical WASM regardless of the checkout path, $HOME, or
# cargo/rustup installation location by remapping absolute paths to stable
# placeholders via --remap-path-prefix. Used by both sync-wasm.sh and CI so
# they cannot drift out of agreement.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME_DIR="${RUSTUP_HOME:-$HOME/.rustup}"

REMAP="--remap-path-prefix=${REPO_ROOT}=/delta"
REMAP="$REMAP --remap-path-prefix=${CARGO_HOME_DIR}=/cargo-home"
REMAP="$REMAP --remap-path-prefix=${RUSTUP_HOME_DIR}=/rustup-home"

export RUSTFLAGS="${RUSTFLAGS:-} $REMAP"

cargo build --release --target wasm32-unknown-unknown "$@"
