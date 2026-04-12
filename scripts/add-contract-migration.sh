#!/bin/bash
# Record the currently-committed site_contract.wasm BLAKE3 hash into
# legacy_contracts.toml so that future releases can migrate sites whose
# state lives under this contract key.
#
# Run this BEFORE rebuilding the contract WASM (i.e. before
# ./scripts/sync-wasm.sh), while ui/public/contracts/site_contract.wasm
# still holds the old release's bytes. Then make the code change, rebuild,
# and commit everything together.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_contracts.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/site_contract.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed contract WASM not found: $COMMITTED"

VERSION="${1:?Usage: add-contract-migration.sh VERSION \"DESCRIPTION\"}"
DESCRIPTION="${2:?Usage: add-contract-migration.sh VERSION \"DESCRIPTION\"}"

CODE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)

echo "Committed contract WASM: $COMMITTED"
echo "  code_hash: $CODE_HASH"

if grep -qF "$CODE_HASH" "$TOML"; then
    echo ""
    echo "This code_hash is already in $TOML — no action needed."
    exit 0
fi

DATE=$(date +%Y-%m-%d)
cat >> "$TOML" << EOF

[[entry]]
version = "$VERSION"
description = "$DESCRIPTION"
date = "$DATE"
code_hash = "$CODE_HASH"
EOF

echo ""
echo "Added $VERSION to $TOML"
echo ""
echo "Next steps:"
echo "  1. Make your code changes (common/, contracts/, etc)"
echo "  2. ./scripts/sync-wasm.sh          # rebuild and copy new WASMs"
echo "  3. cargo test -p delta-ui          # verify build + tests"
echo "  4. git add legacy_contracts.toml ui/public/contracts/"
echo "  5. git commit"
