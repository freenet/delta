#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_delegates.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/site_delegate.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed delegate WASM not found: $COMMITTED"

USAGE='Usage: add-migration.sh VERSION "DESCRIPTION" [--hash <64-hex>]'
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

# --hash records a generation no longer present in the working tree, which is
# the case whenever the new WASM is already committed (publishing after the PR
# merges is a documented order). It is verified against the WASM's real git
# history rather than trusted: an entry recorded for a state that never existed
# satisfies the gate while helping nobody. check-migration.sh prints the exact
# invocation when it refuses.
if [ -n "$EXPLICIT_HASH" ]; then
    [[ "$EXPLICIT_HASH" =~ ^[0-9a-f]{64}$ ]] \
        || die "--hash must be 64 lowercase hex characters, got: $EXPLICIT_HASH"

    CHECK_MIGRATION_LIB_ONLY=1
    export CHECK_MIGRATION_LIB_ONLY
    # shellcheck source=/dev/null
    source "$REPO_ROOT/scripts/check-migration.sh"
    generations_of "ui/public/contracts/site_delegate.wasm" | cut -d' ' -f1 \
        | grep -qxF "$EXPLICIT_HASH" \
        || die "$EXPLICIT_HASH is not a committed state of ui/public/contracts/site_delegate.wasm. Nothing was recorded."

    CODE_HASH="$EXPLICIT_HASH"
else
    CODE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)
fi
DELEGATE_KEY=$(echo -n "$CODE_HASH" | xxd -r -p | b3sum --no-names)

echo "Committed delegate WASM: $COMMITTED"
echo "  code_hash:    $CODE_HASH"
echo "  delegate_key: $DELEGATE_KEY"

# Anchored to a real code_hash assignment. A bare `grep -F` also matches the
# hash inside a commented-out block or quoted in another entry's description,
# and would then skip recording an entry that ui/build.rs (which parses
# [[entry]].code_hash with serde) never sees. Same idiom as
# scripts/check-migration.sh's entry_recorded.
if sed -n 's/^[[:space:]]*code_hash[[:space:]]*=[[:space:]]*"\([0-9a-fA-F]\{64\}\)".*/\1/p' "$TOML" \
    | grep -qxF "$CODE_HASH"; then
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
delegate_key = "$DELEGATE_KEY"
code_hash = "$CODE_HASH"
EOF

echo ""
echo "Added $VERSION to $TOML"

# Stage the file we just wrote, rather than telling the human to remember it.
# An entry that is recorded but never committed is indistinguishable from one
# that was never recorded: the published bundle's migration table lacks the
# outgoing delegate, and every returning user loses access to their signing
# keys and site list. Staging does not guarantee a commit, but it puts the
# change in `git status`'s staged section instead of leaving it to be lost in a
# `git checkout .` or an abandoned working tree.
#
# Deliberately non-fatal: the entry is already written at this point, so a
# staging failure (not a git repo, index lock, …) must not abort the script and
# leave the caller thinking nothing was recorded.
if git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    if git -C "$REPO_ROOT" add "$TOML"; then
        echo "Staged $TOML (staged, NOT committed)."
    else
        echo "WARNING: could not stage $TOML — stage and commit it by hand." >&2
    fi
else
    echo "Not a git repository; skipping staging of $TOML." >&2
fi

echo ""
echo "Next steps:"
echo "  1. ./scripts/sync-wasm.sh       # rebuild and copy new WASMs"
echo "  2. cargo check -p delta-ui      # verify build with new migration entry"
echo "  3. git add ui/public/contracts/ && git commit"
echo ""
echo "The migration entry is worthless until it is COMMITTED and published:"
echo "an unrecorded predecessor means returning users cannot reach their data."
