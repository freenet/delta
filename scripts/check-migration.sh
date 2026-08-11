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
# Testing: `source scripts/check-migration.sh` defines the functions without
# running the checks (sourcing is detected via BASH_SOURCE, see the bottom of
# this file), and CHECK_MIGRATION_REPO_ROOT overrides the repo under
# inspection. Both exist so scripts/tests/check-migration-test.sh can exercise
# the gate against synthetic git repositories without a cargo build. Run that
# test after touching this file.
#
# `CHECK_MIGRATION_LIB_ONLY=1` is accepted when sourcing, for compatibility with
# existing callers, but is now REFUSED when the script is executed directly --
# as an environment variable it could otherwise disarm the gate on the real
# publish path.

REPO_ROOT="${CHECK_MIGRATION_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

DELEGATE_WASM_REL="ui/public/contracts/site_delegate.wasm"
CONTRACT_WASM_REL="ui/public/contracts/site_contract.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

hash_file() { b3sum "$1" | cut -d' ' -f1; }

# Hash of $2's contents as of commit $1.
#   exit 0  -> hash printed
#   exit 2  -> the path did not exist at that commit (legitimate; skip it)
#   dies    -> the object exists but could not be read (corrupt/partial repo)
# The two failures are kept apart on purpose. Collapsing them into one "|| continue"
# turns a broken object store into a silent pass, which is the same fail-open
# shape this gate exists to remove.
blob_hash_at() {
    local commit="$1" path="$2" blob out
    blob=$(git -C "$REPO_ROOT" rev-parse -q --verify "$commit:$path" 2>/dev/null) || return 2
    out=$(git -C "$REPO_ROOT" cat-file blob "$blob" 2>&1 >/dev/null) || \
        die "could not read blob $blob ($path at ${commit:0:12}): $out. The object store is incomplete; refusing to guess."
    git -C "$REPO_ROOT" cat-file blob "$blob" | b3sum | cut -d' ' -f1
}

# Every DISTINCT committed state of $1 on the mainline, newest first, as
# "<hash> <commit>" lines. Fails (dies) rather than returning empty when git
# cannot answer.
#
# Walks git history rather than looking only at HEAD. A HEAD-only comparison
# goes blind the moment the new WASM is committed (HEAD and the working tree
# then agree, so it finds no change to object to), and publishing after the
# commit lands is a documented order -- AGENTS.md's rustc-bump procedure
# publishes in step 8, after the PR merges in step 7. A gate that only works
# when you publish from a dirty working tree is not a gate.
#
# --first-parent keeps the walk on the mainline, so an intermediate WASM that
# only ever existed mid-branch is not demanded as a migration entry.
generations_of() {
    local path="$1" log_out commit hash rc
    log_out=$(git -C "$REPO_ROOT" log --format=%H --first-parent -- "$path" 2>&1) \
        || die "git log failed for $path: $log_out"

    local -A seen=()
    while read -r commit; do
        [ -n "$commit" ] || continue
        hash=$(blob_hash_at "$commit" "$path") || { rc=$?; [ "$rc" = 2 ] && continue; exit "$rc"; }
        [ -n "${seen[$hash]:-}" ] && continue
        seen[$hash]="$commit"
        printf '%s %s\n' "$hash" "$commit"
    done <<< "$log_out"
}

is_shallow() {
    [ "$(git -C "$REPO_ROOT" rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]
}

# True when $1 is recorded as an actual code_hash assignment in TOML $2.
#
# Anchored deliberately. A bare `grep -F <hash>` over the file also matches the
# hash inside a commented-out [[entry]] block, or quoted in some other entry's
# description -- both of which read as "recorded" while ui/build.rs, which
# parses [[entry]].code_hash with serde, never emits them. The sweep would then
# never ask for that hash: silent stranding, reported as safe.
entry_recorded() {
    local hash="$1" toml="$2"
    sed -n 's/^[[:space:]]*code_hash[[:space:]]*=[[:space:]]*"\([0-9a-fA-F]\{64\}\)".*/\1/p' "$toml" \
        | grep -qxF "$hash"
}

# The gate itself.
#   $1 label, $2 WASM path (repo-relative), $3 TOML path (repo-relative),
#   $4 the add-*-migration.sh command that records an entry.
# Exits 1 on any condition it cannot prove safe.
#
# Checks EVERY superseded generation, not just the immediate predecessor.
# Checking one generation is only sound if every earlier generation was itself
# checked when it shipped, which assumes a gate that has always worked and a
# branch protection rule that has always been on. Neither held here: three
# contract generations from April 2026 are unrecorded on main, and a
# predecessor-only gate is structurally incapable of ever noticing them.
# Enumerating every generation is free -- the walk already visits them.
require_generations_recorded() {
    local label="$1" wasm_rel="$2" toml_rel="$3" record_cmd="$4"
    local wasm="$REPO_ROOT/$wasm_rel" toml="$REPO_ROOT/$toml_rel"
    local current line hash commit generations checked=0
    local -a missing=()

    [ -f "$wasm" ] || die "Committed $label WASM not found: $wasm"
    [ -f "$toml" ] || die "Migration table not found: $toml"

    git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1 \
        || die "$REPO_ROOT is not a git repository, so the $label history cannot be read. Refusing to report the gate as passed."

    # A shallow clone can enumerate only the tip, which looks exactly like a
    # clean history. Refuse rather than read that silence as safety.
    if is_shallow; then
        echo ""
        echo "ERROR: cannot audit $label WASM history: this is a shallow clone."
        echo "  Fetch full history before publishing:"
        echo "    git fetch --unshallow"
        echo "  In GitHub Actions, set 'fetch-depth: 0' on actions/checkout."
        exit 1
    fi

    current=$(hash_file "$wasm")
    generations=$(generations_of "$wasm_rel")

    # Zero commits touching the path is NOT "a new artifact with nothing to
    # strand". Both WASMs have been tracked since April 2026, so an empty walk
    # means the walk is broken: the file is untracked, the path was renamed
    # (the walk does not follow renames), HEAD is unborn, or this is an orphan
    # branch. Every one of those previously passed as "first release".
    if [ -z "$generations" ]; then
        echo ""
        echo "ERROR: no committed history found for $wasm_rel."
        echo "  The gate cannot audit a file git has never tracked, so it will not"
        echo "  report success. Likely causes:"
        echo "    - the file is untracked (git add it)"
        echo "    - the path was renamed (this walk does not follow renames;"
        echo "      re-point DELEGATE_WASM_REL/CONTRACT_WASM_REL and re-check by hand)"
        echo "    - HEAD is unborn, or this is an orphan branch with no history"
        exit 1
    fi

    local -A in_history=()
    while read -r hash commit; do
        [ -n "$hash" ] || continue
        checked=$((checked + 1))
        in_history[$hash]=1
        # The current bytes are what this release ships; nothing supersedes
        # them yet, so they are correctly absent from the table.
        [ "$hash" = "$current" ] && continue
        entry_recorded "$hash" "$toml" || missing+=("$hash $commit")
    done <<< "$generations"

    # The converse rule. Every hash the table records is a state that really
    # shipped, so the walk must be able to see it; if it cannot, the walk is
    # looking at the wrong history rather than the table being wrong.
    #
    # This is what catches a RENAMED path, which the emptiness check above
    # cannot: `git log -- <new path>` happily reports the one commit that
    # created it, and `--follow` does not bridge a rename whose content also
    # changed in that commit (verified: similarity detection fails, so the
    # walk still returns exactly one generation). Relocating
    # ui/public/contracts/ would otherwise silently re-baseline the gate to a
    # single generation and pass.
    local -a orphaned=()
    while read -r hash; do
        [ -n "$hash" ] || continue
        [ -n "${in_history[$hash]:-}" ] || orphaned+=("$hash")
    done <<< "$(sed -n 's/^[[:space:]]*code_hash[[:space:]]*=[[:space:]]*"\([0-9a-fA-F]\{64\}\)".*/\1/p' "$toml")"

    if [ ${#orphaned[@]} -gt 0 ]; then
        echo ""
        echo "ERROR: $toml_rel records ${#orphaned[@]} hash(es) that are absent from the"
        echo "committed history of $wasm_rel:"
        for hash in "${orphaned[@]}"; do echo "  $hash"; done
        echo ""
        echo "Every recorded hash is a state that really shipped, so git should be able"
        echo "to show it. Not finding them means this walk is reading the wrong history:"
        echo "the path was renamed or relocated, the history was truncated, or the table"
        echo "was hand-edited. The gate cannot audit what it cannot see, so it refuses."
        exit 1
    fi

    if [ ${#missing[@]} -eq 0 ]; then
        echo "$label: all $checked committed generation(s) accounted for in $toml_rel."
        echo "  current: $current"
        return 0
    fi

    echo ""
    echo "ERROR: ${#missing[@]} superseded $label WASM generation(s) are NOT recorded in $toml_rel"
    for line in "${missing[@]}"; do
        hash=${line%% *}; commit=${line##* }
        echo "  $hash  (commit ${commit:0:12}, $(git -C "$REPO_ROOT" log -1 --format=%ad --date=short "$commit" 2>/dev/null))"
    done
    echo "  current (committed): $current"
    echo ""
    echo "Users whose data lives under any of those is never asked for it: the"
    echo "startup sweep only queries hashes listed in $toml_rel. Their sites"
    echo "disappear silently, with no error and no recovery."
    echo ""
    # --hash, not "git checkout <commit> -- <wasm> && add-*-migration.sh".
    # That recipe silently records the WRONG hash for the contract, whose
    # add-script deliberately reads HEAD's blob and ignores the working tree
    # (verified: it recorded the current hash, not the requested one), and it
    # cannot express this case at all once the new WASM is committed, because
    # then HEAD holds the new bytes too.
    echo "Record each missing generation (the hash is verified against git history):"
    for line in "${missing[@]}"; do
        hash=${line%% *}
        echo "  $record_cmd --hash $hash"
    done
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
    require_generations_recorded "delegate" "$DELEGATE_WASM_REL" "legacy_delegates.toml" \
        './scripts/add-migration.sh V_N "<description>"'

    echo ""
    echo "=== Contract ==="
    require_committed_matches_source "contract" "$CONTRACT_WASM_REL" \
        "site-contract" "site_contract.wasm" \
        './scripts/add-contract-migration.sh C_N "<description>"'
    require_generations_recorded "contract" "$CONTRACT_WASM_REL" "legacy_contracts.toml" \
        './scripts/add-contract-migration.sh C_N "<description>"'

    echo ""
    echo "Safe to publish."
}

# Whether to run the checks is decided by HOW this file was loaded, never by
# the environment. `CHECK_MIGRATION_LIB_ONLY` used to gate this directly, which
# meant an exported copy of an internal test hook silently disarmed the gate on
# the real publish path: cargo-make passes the environment straight through, so
# `CHECK_MIGRATION_LIB_ONLY=1 cargo make publish-delta` exited 0 having checked
# nothing and published anyway. That is the same "guard that cannot fail" shape
# this script exists to remove, so the test hook must not be able to reach it.
#
# `BASH_SOURCE[0]` differs from `$0` exactly when the file was sourced, which is
# the real question and is not settable from outside.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    if [ -n "${CHECK_MIGRATION_LIB_ONLY:-}" ]; then
        die "CHECK_MIGRATION_LIB_ONLY is set while running the gate directly. \
It suppresses every check, so honouring it here would publish unverified. \
Unset it, or 'source' this script instead if you only want its functions."
    fi
    main "$@"
fi
