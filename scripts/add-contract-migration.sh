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

# Append an [[entry]] for $CODE_HASH unless it is already recorded. Defined as
# a function because there are now two ways in: the HEAD-derived path below and
# the explicit --hash path above.
record_entry() {
    # Anchored to a real code_hash assignment; see the matching comment in
    # scripts/add-migration.sh.
    if sed -n 's/^[[:space:]]*code_hash[[:space:]]*=[[:space:]]*"\([0-9a-fA-F]\{64\}\)".*/\1/p' "$TOML" \
        | grep -qxF "$CODE_HASH"; then
        echo ""
        echo "This code_hash is already in $TOML — no action needed."
        exit 0
    fi

    local DATE
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

    # Stage the file we just wrote -- see the matching comment in
    # add-migration.sh. An entry that is recorded but never committed is
    # indistinguishable from one that was never recorded: the published
    # bundle's table lacks the outgoing contract, and sites created under the
    # previous contract key cannot find their state.
    #
    # Deliberately non-fatal: the entry is already written, so a staging
    # failure must not abort the script.
    if git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1; then
        if git -C "$REPO_ROOT" add "$TOML"; then
            echo "Staged $TOML (staged, NOT committed)."
        else
            echo "WARNING: could not stage $TOML -- stage and commit it by hand." >&2
        fi
    else
        echo "Not a git repository; skipping staging of $TOML." >&2
    fi
}

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
command -v git >/dev/null 2>&1 || die "git not found"
[ -f "$COMMITTED" ] || die "Committed contract WASM not found: $COMMITTED"

USAGE='Usage: add-contract-migration.sh VERSION "DESCRIPTION" [--hash <64-hex>]'
VERSION="${1:?$USAGE}"
DESCRIPTION="${2:?$USAGE}"
EXPLICIT_HASH=""
shift 2
while [ $# -gt 0 ]; do
    case "$1" in
        --hash) EXPLICIT_HASH="${2:?--hash needs a value}"; shift 2 ;;
        *) die "Unknown argument: $1. $USAGE" ;;
    esac
done

# --hash records a generation that is no longer reachable from the working
# tree or HEAD. Needed whenever the new WASM is already committed -- publishing
# after the PR merges is a documented order, and then HEAD holds the NEW bytes
# while the predecessor is one commit back, so neither of the paths below can
# reach it. check-migration.sh prints the exact invocation when it refuses.
#
# The hash is verified against the WASM's real git history rather than trusted.
# An unverified --hash would let a typo satisfy the gate while helping nobody:
# an entry recorded for a state that never existed is indistinguishable, to
# every later reader, from one that did.
if [ -n "$EXPLICIT_HASH" ]; then
    [[ "$EXPLICIT_HASH" =~ ^[0-9a-f]{64}$ ]] \
        || die "--hash must be 64 lowercase hex characters, got: $EXPLICIT_HASH"

    CHECK_MIGRATION_LIB_ONLY=1
    export CHECK_MIGRATION_LIB_ONLY
    # shellcheck source=/dev/null
    source "$REPO_ROOT/scripts/check-migration.sh"
    generations_of "ui/public/contracts/site_contract.wasm" | cut -d' ' -f1 \
        | grep -qxF "$EXPLICIT_HASH" \
        || die "$EXPLICIT_HASH is not a committed state of ui/public/contracts/site_contract.wasm. Nothing was recorded."

    CODE_HASH="$EXPLICIT_HASH"
    echo "Recording historical contract WASM generation (verified against git history)"
    echo "  code_hash: $CODE_HASH"
    record_entry
    exit 0
fi

# Always hash the HEAD-tracked WASM, not the working-tree WASM, so that
# running this script after an accidental `sync-wasm.sh` still records
# the *predecessor* hash rather than the fresh one. Otherwise a
# developer who rebuilt before recording would silently leave
# previous-release users stranded.
HEAD_HASH=$(git -C "$REPO_ROOT" show HEAD:ui/public/contracts/site_contract.wasm 2>/dev/null | b3sum 2>/dev/null | cut -d' ' -f1 || true)
WORKTREE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)

if [ -z "$HEAD_HASH" ]; then
    die "Could not read HEAD:ui/public/contracts/site_contract.wasm from git. Is this a git repo with the contract tracked?"
fi

if [ "$HEAD_HASH" != "$WORKTREE_HASH" ]; then
    echo "WARNING: working-tree contract WASM ($WORKTREE_HASH)"
    echo "         differs from HEAD's tracked WASM ($HEAD_HASH)."
    echo ""
    echo "Recording the HEAD hash (the correct predecessor), not the working-tree hash."
    echo "If you meant to record the working-tree hash, revert \`ui/public/contracts/site_contract.wasm\`"
    echo "to HEAD first (git checkout HEAD -- ui/public/contracts/site_contract.wasm) and re-run this script."
fi

CODE_HASH="$HEAD_HASH"

echo "Predecessor contract WASM (from HEAD): $COMMITTED"
echo "  code_hash: $CODE_HASH"

record_entry

echo ""
echo "Next steps:"
echo "  1. Make your code changes (common/, contracts/, etc)"
echo "  2. ./scripts/sync-wasm.sh          # rebuild and copy new WASMs"
echo "  3. cargo test -p delta-ui          # verify build + tests"
echo "  4. git add ui/public/contracts/ && git commit"
