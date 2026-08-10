#!/bin/bash
# Tests for scripts/check-migration.sh's migration gate.
#
# The point of these is narrow and specific: prove the gate can REFUSE. A
# migration gate that runs on the publish path but returns 0 for every input is
# worse than no gate, because it is quoted in the docs as protection. Every
# case below that expects exit 1 is a case where a real release would strand
# every user whose data lives under the previous WASM.
#
# Two cases exist purely as regression guards against the shapes this gate has
# already been broken in:
#   - "committed change, predecessor unrecorded" fails if anyone reverts the
#     history walk to a HEAD-only comparison (HEAD and the working tree agree
#     once the WASM is committed, so a HEAD-only check sees nothing wrong).
#   - "current hash listed, predecessor unrecorded" fails if anyone restores
#     the old 'current hash is in the TOML, therefore fine' short-circuit,
#     which passes whenever add-migration.sh was run one step too late.
#
# No cargo build here: the library is sourced and pointed at synthetic repos,
# so this runs in about a second and can gate every PR.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/../check-migration.sh"

command -v b3sum >/dev/null 2>&1 || { echo "SKIP: b3sum not installed" >&2; exit 1; }

PASS=0
FAIL=0

WASM_REL="ui/public/contracts/site_delegate.wasm"
TOML_REL="legacy_delegates.toml"

# Build a repo whose committed WASM content is $1, with an empty migration
# table. Echoes the repo path.
make_repo() {
    local content="$1" dir
    dir=$(mktemp -d)
    mkdir -p "$dir/ui/public/contracts"
    printf '%s' "$content" > "$dir/$WASM_REL"
    printf '# migration table\n' > "$dir/$TOML_REL"
    git -C "$dir" init -q
    git -C "$dir" config user.email test@example.com
    git -C "$dir" config user.name test
    git -C "$dir" add -A
    git -C "$dir" commit -q -m "initial"
    echo "$dir"
}

hash_of() { printf '%s' "$1" | b3sum | cut -d' ' -f1; }

record() { # $1 repo, $2 content whose hash to record
    printf '\n[[entry]]\nversion = "V1"\ncode_hash = "%s"\n' "$(hash_of "$2")" >> "$1/$TOML_REL"
}

# Run the gate's predecessor check against repo $1 in a subshell (it exits on
# failure) and echo its exit status.
run_gate() {
    local repo="$1"
    (
        CHECK_MIGRATION_LIB_ONLY=1
        CHECK_MIGRATION_REPO_ROOT="$repo"
        export CHECK_MIGRATION_LIB_ONLY CHECK_MIGRATION_REPO_ROOT
        # shellcheck disable=SC1090
        source "$GATE"
        require_predecessor_recorded "delegate" "$WASM_REL" "$TOML_REL" "record-cmd"
    ) >/dev/null 2>&1
    echo $?
}

check() { # $1 description, $2 expected exit, $3 actual exit
    if [ "$2" = "$3" ]; then
        echo "ok   - $1 (exit $3)"
        PASS=$((PASS + 1))
    else
        echo "FAIL - $1: expected exit $2, got $3"
        FAIL=$((FAIL + 1))
    fi
}

# 1. Unchanged WASM: the predecessor is whatever preceded the last change.
#    With no earlier state at all, there is nothing to strand.
repo=$(make_repo "v1")
check "no history: first appearance of the WASM is allowed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# 2. Uncommitted change, predecessor NOT recorded -> must refuse.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$WASM_REL"
check "uncommitted change, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# 3. Uncommitted change, predecessor recorded -> proceed.
repo=$(make_repo "v1")
record "$repo" "v1"
printf 'v2' > "$repo/$WASM_REL"
check "uncommitted change, predecessor recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# 4. REGRESSION GUARD (HEAD-only logic): change already committed, predecessor
#    NOT recorded. A HEAD-only comparison sees HEAD == working tree and passes.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump wasm"
check "committed change, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# 5. Change already committed together with its migration entry -> proceed.
repo=$(make_repo "v1")
record "$repo" "v1"
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump wasm + entry"
check "committed change, predecessor recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# 6. REGRESSION GUARD (the 'already migrated' short-circuit): the CURRENT hash
#    is in the table but the predecessor is not -- what you get by running
#    add-migration.sh after sync-wasm.sh instead of before.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump wasm"
record "$repo" "v2"
check "current hash listed, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# 7. Two generations back is not enough: only the immediate predecessor counts.
repo=$(make_repo "v1")
record "$repo" "v1"
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump to v2"
printf 'v3' > "$repo/$WASM_REL"
check "older generation recorded but immediate predecessor missing: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# 8. Shallow clone: history is truncated before the answer, so the gate must
#    fail closed rather than report the silence as safety.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump wasm"
printf 'v3' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "bump again"
shallow=$(mktemp -d)/clone
git clone -q --depth 1 "file://$repo" "$shallow" 2>/dev/null
if [ -d "$shallow" ] && [ "$(git -C "$shallow" rev-parse --is-shallow-repository)" = "true" ]; then
    # Only one commit present, so the sole committed state equals the current
    # one and no predecessor is visible.
    check "shallow clone, predecessor not in history: REFUSE" 1 "$(run_gate "$shallow")"
else
    echo "FAIL - shallow clone fixture could not be created"
    FAIL=$((FAIL + 1))
fi
rm -rf "$repo" "$shallow"

# 9. The shape CI actually sees. GitHub checks out refs/pull/N/merge, a merge
#    commit whose FIRST parent is the base branch, so the first-parent walk
#    compares the PR's WASM against main's rather than against an intermediate
#    state on the PR branch.
repo=$(make_repo "v1")
git -C "$repo" checkout -q -b pr
printf 'v2' > "$repo/$WASM_REL"
git -C "$repo" commit -q -am "pr: bump wasm"
git -C "$repo" checkout -q -
git -C "$repo" merge -q --no-ff -m "merge pr" pr
check "PR merge commit, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
record "$repo" "v1"
check "PR merge commit, predecessor recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# 10. Not a git repository at all -> refuse, never "safe".
dir=$(mktemp -d)
mkdir -p "$dir/ui/public/contracts"
printf 'v1' > "$dir/$WASM_REL"
printf '# migration table\n' > "$dir/$TOML_REL"
check "not a git repository: REFUSE" 1 "$(run_gate "$dir")"
rm -rf "$dir"

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
