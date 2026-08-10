#!/bin/bash
set -euo pipefail

# The single migration-safety gate for Delta.
#
# Both the delegate and the contract are keyed by the BLAKE3 of their WASM, so
# changing either one orphans every user's stored data unless the OUTGOING hash
# was first recorded in legacy_delegates.toml / legacy_contracts.toml. The UI
# sweeps those tables at startup; a hash that is missing from them is a hash
# nobody ever asks for again, and the user's sites simply disappear with no
# error anywhere.
#
# This script is the ONE implementation of that gate. It runs from:
#   - `cargo make publish-delta` (via the `preflight` -> `check-migration` task)
#   - `.github/workflows/ci.yml` (the "Delegate migration safety" job)
# Do not add a second copy anywhere. A duplicate inline version used to live in
# Makefile.toml, checked something weaker than this, and was what the publish
# path actually ran -- so the documented gate never gated anything (delta#46).
#
# Usage: ./scripts/check-migration.sh
# Exit 0 = safe to publish, exit 1 = a migration entry is missing (or the
# committed WASM is stale, or history is too shallow to tell).
#
# Testing: `CHECK_MIGRATION_LIB_ONLY=1 source scripts/check-migration.sh`
# defines the functions without running the checks, and
# CHECK_MIGRATION_REPO_ROOT overrides the repo under inspection. Both exist so
# scripts/tests/check-migration-test.sh can exercise the gate against synthetic
# git repositories without a cargo build. Run that test after touching this file.

REPO_ROOT="${CHECK_MIGRATION_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

DELEGATE_WASM_REL="ui/public/contracts/site_delegate.wasm"
CONTRACT_WASM_REL="ui/public/contracts/site_contract.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

hash_file() { b3sum "$1" | cut -d' ' -f1; }

# Hash of $2's contents as of commit $1, or non-zero if the path did not exist
# there. Goes through `rev-parse`/`cat-file` rather than `git show` so that a
# missing path is a failed lookup instead of an empty stream silently hashing
# to the BLAKE3 of nothing.
blob_hash_at() {
    local commit="$1" path="$2" blob
    blob=$(git -C "$REPO_ROOT" rev-parse -q --verify "$commit:$path" 2>/dev/null) || return 1
    git -C "$REPO_ROOT" cat-file blob "$blob" | b3sum | cut -d' ' -f1
}

# Echo "<hash> <commit>" for the most recent committed state of $1 whose
# contents differ from the current hash $2 -- i.e. the release this one
# supersedes. Returns 1 when no such ancestor exists.
#
# Deliberately walks git history rather than looking only at HEAD. A HEAD-only
# comparison goes blind the moment the new WASM is committed (HEAD and the
# working tree then agree, so it finds no change to object to), and publishing
# after the commit lands is a documented order -- AGENTS.md's rustc-bump
# procedure publishes in step 8, after the PR merges in step 7. A gate that
# only works when you publish from a dirty working tree is not a gate.
#
# --first-parent keeps the walk on the mainline, so an intermediate WASM that
# only ever existed mid-branch is not demanded as a migration entry.
predecessor_of() {
    local path="$1" current="$2" commit hash
    while read -r commit; do
        [ -n "$commit" ] || continue
        hash=$(blob_hash_at "$commit" "$path") || continue
        if [ "$hash" != "$current" ]; then
            printf '%s %s\n' "$hash" "$commit"
            return 0
        fi
    done < <(git -C "$REPO_ROOT" log --format=%H --first-parent -- "$path" 2>/dev/null)
    return 1
}

is_shallow() {
    [ "$(git -C "$REPO_ROOT" rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]
}

# The gate itself.
#   $1 label, $2 WASM path (repo-relative), $3 TOML path (repo-relative),
#   $4 the add-*-migration.sh command that records an entry.
# Exits 1 on any condition it cannot prove safe.
require_predecessor_recorded() {
    local label="$1" wasm_rel="$2" toml_rel="$3" record_cmd="$4"
    local wasm="$REPO_ROOT/$wasm_rel" toml="$REPO_ROOT/$toml_rel"
    local current predecessor prev_hash prev_commit

    [ -f "$wasm" ] || die "Committed $label WASM not found: $wasm"
    [ -f "$toml" ] || die "Migration table not found: $toml"

    current=$(hash_file "$wasm")

    git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1 \
        || die "$REPO_ROOT is not a git repository, so the $label predecessor cannot be determined. Refusing to report the gate as passed."

    if predecessor=$(predecessor_of "$wasm_rel" "$current"); then
        prev_hash=${predecessor%% *}
        prev_commit=${predecessor##* }
    else
        # No differing ancestor. In a full clone that means this is the WASM's
        # first appearance and there is no predecessor to strand. In a shallow
        # clone it means the history was truncated before the answer -- which
        # is indistinguishable from "safe" unless we refuse to guess.
        if is_shallow; then
            echo ""
            echo "ERROR: cannot determine the previous $label WASM hash: this is a shallow clone."
            echo "  The gate needs enough history to find the commit that last changed"
            echo "  $wasm_rel. Fetch it before publishing:"
            echo "    git fetch --unshallow"
            echo "  In GitHub Actions, set 'fetch-depth: 0' on actions/checkout."
            exit 1
        fi
        echo "No earlier committed $label WASM in history (first release of this artifact)."
        return 0
    fi

    if grep -qF "$prev_hash" "$toml"; then
        echo "$label WASM predecessor is recorded in $toml_rel."
        echo "  predecessor: $prev_hash"
        echo "  current:     $current"
        return 0
    fi

    echo ""
    echo "ERROR: the previous $label WASM hash is NOT recorded in $toml_rel"
    echo "  predecessor (commit ${prev_commit:0:12}): $prev_hash"
    echo "  current (committed):                  $current"
    echo ""
    echo "Users whose data lives under the previous $label will never be asked for it:"
    echo "the startup sweep only queries hashes listed in $toml_rel. Their sites"
    echo "disappear silently, with no error and no recovery."
    echo ""
    echo "Record the predecessor, then rebuild:"
    echo "  git checkout $prev_commit -- $wasm_rel"
    echo "  $record_cmd"
    echo "  ./scripts/sync-wasm.sh"
    exit 1
}

# Rebuild from source and refuse if the committed bytes are not what this
# toolchain produces -- otherwise every hash below describes a file nobody can
# reproduce, and the predecessor check is comparing fiction.
require_committed_matches_source() {
    local label="$1" wasm_rel="$2" package="$3" built_name="$4" record_cmd="$5"
    local committed built

    committed=$(hash_file "$REPO_ROOT/$wasm_rel")

    echo "Building $label WASM from source..."
    "$REPO_ROOT/scripts/build-wasm.sh" -p "$package" >/dev/null 2>&1 \
        || die "Failed to build $package from source."
    built=$(hash_file "$REPO_ROOT/target/wasm32-unknown-unknown/release/$built_name")

    if [ "$committed" != "$built" ]; then
        echo ""
        echo "ERROR: committed $label WASM does not match what this toolchain built from source."
        echo "  committed: $committed"
        echo "  built:     $built"
        echo ""
        echo "If you changed $label code, record the outgoing hash first, then rebuild:"
        echo "  $record_cmd"
        echo "  ./scripts/sync-wasm.sh"
        echo ""
        echo "If no such change is in this diff, the toolchain has drifted from"
        echo "rust-toolchain.toml. Compare 'rustc --version --verbose' against:"
        echo "  $(grep '^channel' "$REPO_ROOT/rust-toolchain.toml" 2>/dev/null || echo '(rust-toolchain.toml not found)')"
        exit 1
    fi
    echo "Committed $label WASM matches source (hash: ${committed:0:16}...)."
}

main() {
    command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
    command -v git >/dev/null 2>&1 || die "git not found"

    echo "=== Delegate ==="
    require_committed_matches_source "delegate" "$DELEGATE_WASM_REL" \
        "site-delegate" "site_delegate.wasm" \
        './scripts/add-migration.sh V_N "<description>"'
    require_predecessor_recorded "delegate" "$DELEGATE_WASM_REL" "legacy_delegates.toml" \
        './scripts/add-migration.sh V_N "<description>"'

    echo ""
    echo "=== Contract ==="
    require_committed_matches_source "contract" "$CONTRACT_WASM_REL" \
        "site-contract" "site_contract.wasm" \
        './scripts/add-contract-migration.sh C_N "<description>"'
    require_predecessor_recorded "contract" "$CONTRACT_WASM_REL" "legacy_contracts.toml" \
        './scripts/add-contract-migration.sh C_N "<description>"'

    echo ""
    echo "Safe to publish."
}

if [ -z "${CHECK_MIGRATION_LIB_ONLY:-}" ]; then
    main "$@"
fi
