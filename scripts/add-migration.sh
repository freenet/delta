#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_delegates.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/site_delegate.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed delegate WASM not found: $COMMITTED"

VERSION="${1:?Usage: add-migration.sh VERSION \"DESCRIPTION\"}"
DESCRIPTION="${2:?Usage: add-migration.sh VERSION \"DESCRIPTION\"}"

CODE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)
DELEGATE_KEY=$(echo -n "$CODE_HASH" | xxd -r -p | b3sum --no-names)

echo "Committed delegate WASM: $COMMITTED"
echo "  code_hash:    $CODE_HASH"
echo "  delegate_key: $DELEGATE_KEY"

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
