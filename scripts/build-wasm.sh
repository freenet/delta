#!/bin/bash
# Reproducible WASM build for site-contract and site-delegate.
#
# Produces byte-identical WASM regardless of the checkout path, $HOME, or
# cargo/rustup installation location by remapping absolute paths to stable
# placeholders via --remap-path-prefix. Used by both sync-wasm.sh and CI so
# they cannot drift out of agreement.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$REPO_ROOT"

# Canonicalize to match what rustc sees (it resolves symlinks on source
# paths), and strip trailing slashes so --remap-path-prefix literal
# matching isn't thrown off by CARGO_HOME=/foo/ vs /foo.
CARGO_HOME_DIR="$(realpath -m "${CARGO_HOME:-${HOME:?HOME or CARGO_HOME must be set}/.cargo}")"
RUSTUP_HOME_DIR="$(realpath -m "${RUSTUP_HOME:-${HOME:?HOME or RUSTUP_HOME must be set}/.rustup}")"

# When the rust-src component is installed, rustc substitutes the real
# on-disk path ($SYSROOT/lib/rustlib/src/rust/...) for std/core panics.
# Without rust-src (e.g. in CI), rustc keeps the virtual /rustc/<commit>/
# path that's baked into the pre-compiled std. Remap the real path to
# the virtual one so both environments emit identical bytes.
RUSTC_COMMIT="$(rustc --version --verbose | sed -n 's/^commit-hash: //p')"
RUST_SYSROOT="$(rustc --print sysroot)"
RUST_SRC_DIR="${RUST_SYSROOT}/lib/rustlib/src/rust"

REMAP="--remap-path-prefix=${REPO_ROOT}=/delta"
REMAP="$REMAP --remap-path-prefix=${CARGO_HOME_DIR}=/cargo-home"
REMAP="$REMAP --remap-path-prefix=${RUST_SRC_DIR}=/rustc/${RUSTC_COMMIT}"
REMAP="$REMAP --remap-path-prefix=${RUSTUP_HOME_DIR}=/rustup-home"

export RUSTFLAGS="${RUSTFLAGS:-} $REMAP"

cargo build --release --target wasm32-unknown-unknown "$@"
