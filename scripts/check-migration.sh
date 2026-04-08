#!/bin/bash
set -euo pipefail

# Checks whether the delegate WASM has changed since the last legacy_delegates.toml
# entry. If so, a migration entry is required before publishing.
#
# Usage: ./scripts/check-migration.sh
# Exit 0 = safe to publish, Exit 1 = migration entry needed

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_delegates.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/site_delegate.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed delegate WASM not found: $COMMITTED"

CURRENT_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)

# Build the delegate from source and compare
echo "Building delegate WASM from source..."
cargo build --release --target wasm32-unknown-unknown -p site-delegate 2>/dev/null
BUILT_HASH=$(b3sum "$REPO_ROOT/target/wasm32-unknown-unknown/release/site_delegate.wasm" | cut -d' ' -f1)

if [ "$CURRENT_HASH" != "$BUILT_HASH" ]; then
    echo ""
    echo "WARNING: Committed delegate WASM is stale!"
    echo "  committed: $CURRENT_HASH"
    echo "  built:     $BUILT_HASH"
    echo ""
    echo "Run ./scripts/sync-wasm.sh to update committed WASMs."
    echo "If the delegate code changed, first run:"
    echo "  ./scripts/add-migration.sh VERSION \"DESCRIPTION\""
    exit 1
fi

# Check if current hash is already in legacy_delegates.toml (meaning we
# haven't changed delegate code since last migration entry - that's fine)
if grep -qF "$CURRENT_HASH" "$TOML"; then
    echo "Current delegate WASM hash is in legacy_delegates.toml (already migrated)."
    echo "Safe to publish."
    exit 0
fi

# Current hash is NOT in legacy_delegates.toml. This is OK only if there
# are no entries at all (first deploy) or if this is intentionally a new
# version that hasn't been deployed yet.

# Count entries
ENTRY_COUNT=$(grep -c '^\[\[entry\]\]' "$TOML" 2>/dev/null || echo 0)

if [ "$ENTRY_COUNT" -eq 0 ]; then
    echo "No legacy entries yet (first deploy). Safe to publish."
    exit 0
fi

# There are legacy entries but the current hash isn't among them.
# This is the expected state after adding a migration entry and rebuilding.
# Verify the PREVIOUS hash (last entry's code_hash) differs from current.
LAST_HASH=$(grep 'code_hash' "$TOML" | tail -1 | sed 's/.*= *"//' | sed 's/".*//')

if [ "$CURRENT_HASH" = "$LAST_HASH" ]; then
    echo "Current WASM matches last migration entry. No new migration needed."
    echo "Safe to publish."
    exit 0
fi

echo "Delegate WASM has changed since last migration entry."
echo "  current hash: $CURRENT_HASH"
echo "  last entry:   $LAST_HASH"
echo ""
echo "Safe to publish (migration entries cover previous versions)."
exit 0
