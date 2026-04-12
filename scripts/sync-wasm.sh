#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "Building site-contract and site-delegate for wasm32..."
"$REPO_ROOT/scripts/build-wasm.sh" -p site-contract -p site-delegate

echo "Copying WASMs to committed location..."
cp target/wasm32-unknown-unknown/release/site_contract.wasm ui/public/contracts/
cp target/wasm32-unknown-unknown/release/site_delegate.wasm ui/public/contracts/

echo "Done."
echo ""
echo "WASM sizes:"
ls -la ui/public/contracts/*.wasm
