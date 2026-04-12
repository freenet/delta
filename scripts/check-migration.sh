#!/bin/bash
set -euo pipefail

# Checks whether the delegate or contract WASM has changed since the last
# legacy_delegates.toml / legacy_contracts.toml entry. If so, a migration
# entry is required before publishing.
#
# Usage: ./scripts/check-migration.sh
# Exit 0 = safe to publish, Exit 1 = migration entry needed

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DELEGATE_TOML="$REPO_ROOT/legacy_delegates.toml"
CONTRACT_TOML="$REPO_ROOT/legacy_contracts.toml"
DELEGATE_WASM="$REPO_ROOT/ui/public/contracts/site_delegate.wasm"
CONTRACT_WASM="$REPO_ROOT/ui/public/contracts/site_contract.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$DELEGATE_WASM" ] || die "Committed delegate WASM not found: $DELEGATE_WASM"
[ -f "$CONTRACT_WASM" ] || die "Committed contract WASM not found: $CONTRACT_WASM"

# -- Delegate check -----------------------------------------------------------

CURRENT_DELEGATE_HASH=$(b3sum "$DELEGATE_WASM" | cut -d' ' -f1)

echo "Building delegate WASM from source..."
"$REPO_ROOT/scripts/build-wasm.sh" -p site-delegate 2>/dev/null
BUILT_DELEGATE_HASH=$(b3sum "$REPO_ROOT/target/wasm32-unknown-unknown/release/site_delegate.wasm" | cut -d' ' -f1)

if [ "$CURRENT_DELEGATE_HASH" != "$BUILT_DELEGATE_HASH" ]; then
    echo ""
    echo "WARNING: Committed delegate WASM is stale!"
    echo "  committed: $CURRENT_DELEGATE_HASH"
    echo "  built:     $BUILT_DELEGATE_HASH"
    echo ""
    echo "Run ./scripts/sync-wasm.sh to update committed WASMs."
    echo "If the delegate code changed, first run:"
    echo "  ./scripts/add-migration.sh VERSION \"DESCRIPTION\""
    exit 1
fi

if grep -qF "$CURRENT_DELEGATE_HASH" "$DELEGATE_TOML"; then
    echo "Current delegate WASM hash is in legacy_delegates.toml (already migrated)."
else
    DELEGATE_ENTRY_COUNT=$(grep -c '^\[\[entry\]\]' "$DELEGATE_TOML" 2>/dev/null || echo 0)
    if [ "$DELEGATE_ENTRY_COUNT" -eq 0 ]; then
        echo "No legacy delegate entries yet (first deploy)."
    else
        LAST_DELEGATE_HASH=$(grep 'code_hash' "$DELEGATE_TOML" | tail -1 | sed 's/.*= *"//' | sed 's/".*//')
        if [ "$CURRENT_DELEGATE_HASH" = "$LAST_DELEGATE_HASH" ]; then
            echo "Current delegate WASM matches last delegate migration entry."
        else
            echo "Delegate WASM has changed since last migration entry."
            echo "  current hash: $CURRENT_DELEGATE_HASH"
            echo "  last entry:   $LAST_DELEGATE_HASH"
        fi
    fi
fi

# -- Contract check -----------------------------------------------------------

CURRENT_CONTRACT_HASH=$(b3sum "$CONTRACT_WASM" | cut -d' ' -f1)

echo ""
echo "Building contract WASM from source..."
"$REPO_ROOT/scripts/build-wasm.sh" -p site-contract 2>/dev/null
BUILT_CONTRACT_HASH=$(b3sum "$REPO_ROOT/target/wasm32-unknown-unknown/release/site_contract.wasm" | cut -d' ' -f1)

if [ "$CURRENT_CONTRACT_HASH" != "$BUILT_CONTRACT_HASH" ]; then
    echo ""
    echo "WARNING: Committed contract WASM is stale!"
    echo "  committed: $CURRENT_CONTRACT_HASH"
    echo "  built:     $BUILT_CONTRACT_HASH"
    echo ""
    echo "Run ./scripts/sync-wasm.sh to update committed WASMs."
    echo "If common/ or the contract source changed, first run:"
    echo "  ./scripts/add-contract-migration.sh VERSION \"DESCRIPTION\""
    exit 1
fi

# The contract check is a strict one-directional rule: if the CURRENT
# committed hash is NOT in legacy_contracts.toml, then the PREVIOUS
# committed hash (from git HEAD) must be, i.e. the release that's
# about to ship recorded its predecessor before rebuilding. We detect
# the "forgot to record" case by checking whether the previous
# git-tracked hash of site_contract.wasm is present in the TOML.

CONTRACT_ENTRY_COUNT=$(grep -c '^\[\[entry\]\]' "$CONTRACT_TOML" 2>/dev/null || echo 0)

if [ "$CONTRACT_ENTRY_COUNT" -eq 0 ]; then
    echo "No legacy contract entries yet (first deploy)."
    echo ""
    echo "Safe to publish."
    exit 0
fi

if grep -qF "$CURRENT_CONTRACT_HASH" "$CONTRACT_TOML"; then
    echo "Current contract WASM hash is in legacy_contracts.toml."
    echo ""
    echo "Safe to publish."
    exit 0
fi

PREVIOUS_CONTRACT_HASH=$(git -C "$REPO_ROOT" show HEAD:ui/public/contracts/site_contract.wasm 2>/dev/null | b3sum 2>/dev/null | cut -d' ' -f1 || true)

if [ -z "$PREVIOUS_CONTRACT_HASH" ]; then
    echo "Could not determine previous contract WASM hash from git; skipping strict check."
elif [ "$PREVIOUS_CONTRACT_HASH" = "$CURRENT_CONTRACT_HASH" ]; then
    echo "Contract WASM unchanged from HEAD."
elif grep -qF "$PREVIOUS_CONTRACT_HASH" "$CONTRACT_TOML"; then
    echo "Contract WASM changed; previous hash is recorded in legacy_contracts.toml."
    echo "  previous: $PREVIOUS_CONTRACT_HASH"
    echo "  current:  $CURRENT_CONTRACT_HASH"
else
    echo ""
    echo "ERROR: Contract WASM changed but the previous hash is NOT in legacy_contracts.toml"
    echo "  previous (from HEAD): $PREVIOUS_CONTRACT_HASH"
    echo "  current (committed):  $CURRENT_CONTRACT_HASH"
    echo ""
    echo "Sites created with the previous release will be unable to find their state."
    echo "Record the previous hash before rebuilding:"
    echo "  git checkout HEAD -- ui/public/contracts/site_contract.wasm"
    echo "  ./scripts/add-contract-migration.sh V_N 'DESCRIPTION'"
    echo "  ./scripts/sync-wasm.sh"
    exit 1
fi

echo ""
echo "Safe to publish."
exit 0
