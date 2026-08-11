#!/bin/bash
# Tests for the migration gate (scripts/check-migration.sh).
#
# The point is narrow and specific: prove the gate can REFUSE, and prove it is
# WIRED IN. A gate that runs on the publish path but returns 0 for every input
# is worse than no gate, because the docs cite it as protection.
#
# Three layers, because the first two have each been the actual bug here:
#
#   WIRING   (source scrapes) - Makefile.toml delegates to the script instead
#            of reimplementing it, preflight depends on it, publish-delta goes
#            through preflight, CI runs it with full history. The ORIGINAL bug
#            was wiring: a Makefile copy bypassed the script entirely, so every
#            behavioural test of the script passed while nothing gated publish.
#
#   END-TO-END - run the REAL script against synthetic repos with a stubbed
#            build-wasm.sh. These exercise main(), so deleting a check from it
#            fails here. An earlier version of this file only called the helper
#            directly, and passed 11/11 with both calls deleted from main().
#
#   UNIT     - call the helper directly for the many history shapes.
#
# No cargo build: about a second, so it gates every PR.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/../check-migration.sh"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Exits 1, so call it what it is. A missing b3sum means the gate cannot hash a
# WASM and so cannot answer the question, which must block a publish rather
# than be reported as a skipped nicety.
command -v b3sum >/dev/null 2>&1 || { echo "FAIL: b3sum not installed — the gate cannot run" >&2; exit 1; }

PASS=0
FAIL=0

DELEGATE_REL="ui/public/contracts/site_delegate.wasm"
CONTRACT_REL="ui/public/contracts/site_contract.wasm"
DELEGATE_TOML="legacy_delegates.toml"
CONTRACT_TOML="legacy_contracts.toml"

check() { # $1 description, $2 expected exit, $3 actual exit
    if [ "$2" = "$3" ]; then
        echo "ok   - $1 (exit $3)"
        PASS=$((PASS + 1))
    else
        echo "FAIL - $1: expected exit $2, got $3"
        FAIL=$((FAIL + 1))
    fi
}

assert() { # $1 description, $2 = 0/1 truth of the condition
    check "$1" 0 "$2"
}

hash_of() { printf '%s' "$1" | b3sum | cut -d' ' -f1; }

git_init() {
    git -C "$1" init -q
    git -C "$1" config user.email test@example.com
    git -C "$1" config user.name test
}

record() { # $1 repo, $2 toml, $3 content whose hash to record
    printf '\n[[entry]]\nversion = "V1"\ncode_hash = "%s"\n' "$(hash_of "$3")" >> "$1/$2"
}

# ---------------------------------------------------------------- WIRING ----
# These read the real Makefile.toml / ci.yml. They are the layer that would
# have caught the original bug, in which the script was correct and unused.

makefile_task() { # $1 task name -> that task's block
    awk -v t="[tasks.$1]" '$0==t{f=1;next} /^\[/{f=0} f' "$REPO/Makefile.toml"
}

echo "--- wiring ---"

task_body=$(makefile_task check-migration)
grep -q '^script = "\./scripts/check-migration\.sh"$' <<< "$task_body"
assert "Makefile check-migration delegates to the script" $?

# The inline copy that caused delta#46 hashed WASMs itself. Any reappearance of
# hashing or WASM paths in this task means a second implementation is back.
! grep -qE 'b3sum|site_delegate\.wasm|legacy_delegates\.toml' <<< "$task_body"
assert "Makefile check-migration contains no inline reimplementation" $?

preflight_body=$(makefile_task preflight)
grep -q 'dependencies = .*"check-migration"' <<< "$preflight_body"
assert "preflight depends on check-migration" $?

grep -q 'dependencies = .*"test-migration-gate"' <<< "$preflight_body"
assert "preflight depends on test-migration-gate (this file)" $?

grep -q 'dependencies = .*"preflight"' <<< "$(makefile_task publish-delta)"
assert "publish-delta depends on preflight" $?

# Depending on the gate is not the same as being stopped by it. cargo-make's
# `ignore_errors`/`force` make a task's non-zero exit non-fatal, so either one
# on any task in this chain would let a REFUSING gate be walked straight past
# while every other assertion here stayed green.
chain_honours_failure=0
for t in check-migration test-migration-gate preflight publish-delta; do
    if grep -qE '^(ignore_errors|force) *= *true' <<< "$(makefile_task "$t")"; then
        echo "  [$t] sets ignore_errors/force, so a failing gate would not stop it" >&2
        chain_honours_failure=1
    fi
done
assert "no task in the publish chain ignores a failing dependency" $chain_honours_failure

ci="$REPO/.github/workflows/ci.yml"
grep -q 'run: \./scripts/check-migration\.sh' "$ci"
assert "CI runs the gate" $?

grep -q 'fetch-depth: 0' "$ci"
assert "CI checks out full history (shallow would disarm the gate)" $?

grep -q 'run: \./scripts/tests/check-migration-test\.sh' "$ci"
assert "CI runs this test file" $?

# The gate must decide whether to run from HOW it was loaded, never from the
# environment. `CHECK_MIGRATION_LIB_ONLY` once gated `main` directly, so an
# exported copy of this internal test hook made the gate a silent no-op on the
# real publish path (cargo-make passes the environment through untouched):
# `CHECK_MIGRATION_LIB_ONLY=1 cargo make publish-delta` exited 0 having checked
# nothing. Executing must refuse; sourcing must still yield the functions.
CHECK_MIGRATION_LIB_ONLY=1 bash "$GATE" >/dev/null 2>&1
[ $? -ne 0 ]
assert "executing the gate with CHECK_MIGRATION_LIB_ONLY set refuses" $?

bash -c "CHECK_MIGRATION_LIB_ONLY=1 source '$GATE' && [ \"\$(type -t require_generations_recorded)\" = function ]" >/dev/null 2>&1
assert "sourcing the gate still defines its functions without running checks" $?

# ------------------------------------------------------------ END-TO-END ----
# Run the REAL script, via main(), against a synthetic repo. build-wasm.sh is
# stubbed so no cargo build is needed.

make_e2e_repo() { # echoes repo path; both WASMs contain "v1"
    local dir
    dir=$(mktemp -d)
    mkdir -p "$dir/ui/public/contracts" "$dir/scripts"
    printf 'v1' > "$dir/$DELEGATE_REL"
    printf 'v1' > "$dir/$CONTRACT_REL"
    printf '# delegate table\n' > "$dir/$DELEGATE_TOML"
    printf '# contract table\n' > "$dir/$CONTRACT_TOML"
    cat > "$dir/scripts/build-wasm.sh" <<'STUB'
#!/bin/bash
# Test stub: "builds" by copying the committed WASM, so committed == built.
# Touching .drift-<name> makes it emit different bytes, simulating a source
# change that was never synced.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/target/wasm32-unknown-unknown/release"
while [ $# -gt 0 ]; do
    case "$1" in
        -p)
            case "$2" in
                site-delegate) name=site_delegate ;;
                site-contract) name=site_contract ;;
                *) shift 2; continue ;;
            esac
            cp "$root/ui/public/contracts/$name.wasm" \
               "$root/target/wasm32-unknown-unknown/release/$name.wasm"
            if [ -f "$root/.drift-$name" ]; then
                printf 'drift' >> "$root/target/wasm32-unknown-unknown/release/$name.wasm"
            fi
            shift 2 ;;
        *) shift ;;
    esac
done
STUB
    chmod +x "$dir/scripts/build-wasm.sh"
    git_init "$dir"
    git -C "$dir" add -A
    git -C "$dir" commit -q -m initial
    echo "$dir"
}

run_e2e() { CHECK_MIGRATION_REPO_ROOT="$1" bash "$GATE" >/dev/null 2>&1; echo $?; }

echo "--- end-to-end (exercises main()) ---"

repo=$(make_e2e_repo)
check "e2e: everything current and consistent: proceed" 0 "$(run_e2e "$repo")"
rm -rf "$repo"

# Deleting the delegate check from main() makes this pass when it must not.
repo=$(make_e2e_repo)
printf 'v2' > "$repo/$DELEGATE_REL"
check "e2e: delegate predecessor unrecorded: REFUSE" 1 "$(run_e2e "$repo")"
rm -rf "$repo"

# Deleting the contract check from main() makes this pass when it must not.
repo=$(make_e2e_repo)
printf 'v2' > "$repo/$CONTRACT_REL"
check "e2e: contract predecessor unrecorded: REFUSE" 1 "$(run_e2e "$repo")"
rm -rf "$repo"

repo=$(make_e2e_repo)
printf 'v2' > "$repo/$DELEGATE_REL"
record "$repo" "$DELEGATE_TOML" "v1"
check "e2e: delegate predecessor recorded: proceed" 0 "$(run_e2e "$repo")"
rm -rf "$repo"

# Deleting require_committed_matches_source from main() makes this pass.
repo=$(make_e2e_repo)
touch "$repo/.drift-site_delegate"
check "e2e: committed delegate WASM stale vs source: REFUSE" 1 "$(run_e2e "$repo")"
rm -rf "$repo"

repo=$(make_e2e_repo)
touch "$repo/.drift-site_contract"
check "e2e: committed contract WASM stale vs source: REFUSE" 1 "$(run_e2e "$repo")"
rm -rf "$repo"

# ------------------------------------------------------------------ UNIT ----

make_repo() { # $1 initial content
    local dir
    dir=$(mktemp -d)
    mkdir -p "$dir/ui/public/contracts"
    printf '%s' "$1" > "$dir/$DELEGATE_REL"
    printf '# migration table\n' > "$dir/$DELEGATE_TOML"
    git_init "$dir"
    git -C "$dir" add -A
    git -C "$dir" commit -q -m initial
    echo "$dir"
}

run_gate() {
    (
        CHECK_MIGRATION_LIB_ONLY=1
        CHECK_MIGRATION_REPO_ROOT="$1"
        export CHECK_MIGRATION_LIB_ONLY CHECK_MIGRATION_REPO_ROOT
        # shellcheck disable=SC1090
        source "$GATE"
        require_generations_recorded "delegate" "$DELEGATE_REL" "$DELEGATE_TOML" "record-cmd"
    ) >/dev/null 2>&1
    echo $?
}

echo "--- unit (history shapes) ---"

repo=$(make_repo "v1")
check "single generation, equal to current: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
check "uncommitted change, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

repo=$(make_repo "v1")
record "$repo" "$DELEGATE_TOML" "v1"
printf 'v2' > "$repo/$DELEGATE_REL"
check "uncommitted change, predecessor recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# REGRESSION GUARD (HEAD-only logic): a HEAD-only comparison sees HEAD ==
# working tree once the WASM is committed, and passes.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "bump wasm"
check "committed change, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

repo=$(make_repo "v1")
record "$repo" "$DELEGATE_TOML" "v1"
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "bump wasm + entry"
check "committed change, predecessor recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# REGRESSION GUARD (the "already migrated" short-circuit): the CURRENT hash is
# listed but the predecessor is not -- add-migration.sh run one step too late.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "bump wasm"
record "$repo" "$DELEGATE_TOML" "v2"
check "current hash listed, predecessor unrecorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# REGRESSION GUARD (predecessor-only logic): this is the shape of the three
# unrecorded April 2026 contract generations on main. The immediate
# predecessor is recorded, so a gate that stops at the first differing state
# passes while an older generation stays permanently unreachable.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "to v2"            # v1 never recorded
record "$repo" "$DELEGATE_TOML" "v2"
printf 'v3' > "$repo/$DELEGATE_REL"
check "older generation unrecorded, immediate predecessor recorded: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

repo=$(make_repo "v1")
record "$repo" "$DELEGATE_TOML" "v1"
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "to v2"
record "$repo" "$DELEGATE_TOML" "v2"
printf 'v3' > "$repo/$DELEGATE_REL"
check "every superseded generation recorded: proceed" 0 "$(run_gate "$repo")"
rm -rf "$repo"

# --- the TOML must be read the way ui/build.rs reads it ---

# A commented-out entry is not an entry: serde never sees it, so the sweep
# never asks for that hash. An unanchored `grep -F` matches it anyway.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
printf '\n# [[entry]]\n# code_hash = "%s"\n' "$(hash_of v1)" >> "$repo/$DELEGATE_TOML"
check "hash only in a commented-out entry: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# Same for a hash quoted in some other entry's description.
repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
printf '\n[[entry]]\nversion = "VX"\ndescription = "supersedes %s"\ncode_hash = "%s"\n' \
    "$(hash_of v1)" "$(hash_of zzz)" >> "$repo/$DELEGATE_TOML"
check "hash only inside another entry's description: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# --- fail-closed: the gate must refuse whenever it cannot see the history ---

repo=$(make_repo "v1")
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "bump wasm"
printf 'v3' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "bump again"
shallow=$(mktemp -d)/clone
git clone -q --depth 1 "file://$repo" "$shallow" 2>/dev/null
if [ -d "$shallow" ] && [ "$(git -C "$shallow" rev-parse --is-shallow-repository)" = "true" ]; then
    check "shallow clone: REFUSE" 1 "$(run_gate "$shallow")"
else
    echo "FAIL - shallow clone fixture could not be created"; FAIL=$((FAIL + 1))
fi
rm -rf "$repo" "$shallow"

# WASM present on disk but never committed.
dir=$(mktemp -d)
mkdir -p "$dir/ui/public/contracts"
printf 'v1' > "$dir/$DELEGATE_REL"
printf '# migration table\n' > "$dir/$DELEGATE_TOML"
git_init "$dir"
git -C "$dir" add "$DELEGATE_TOML"
git -C "$dir" commit -q -m "toml only"
check "WASM untracked by git: REFUSE" 1 "$(run_gate "$dir")"
rm -rf "$dir"

# A repository with no commits at all: `git log` is fatal, and swallowing that
# used to look like "no history, therefore first release".
dir=$(mktemp -d)
mkdir -p "$dir/ui/public/contracts"
printf 'v1' > "$dir/$DELEGATE_REL"
printf '# migration table\n' > "$dir/$DELEGATE_TOML"
git_init "$dir"
check "repository with no commits (unborn HEAD): REFUSE" 1 "$(run_gate "$dir")"
rm -rf "$dir"

# Orphan branch: history exists, but not on this branch.
repo=$(make_repo "v1")
record "$repo" "$DELEGATE_TOML" "v1"
git -C "$repo" commit -q -am "record v1"
git -C "$repo" checkout -q --orphan fresh
git -C "$repo" rm -rq --cached . 2>/dev/null
printf 'v2' > "$repo/$DELEGATE_REL"
check "orphan branch with no history: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# Not a git repository at all.
dir=$(mktemp -d)
mkdir -p "$dir/ui/public/contracts"
printf 'v1' > "$dir/$DELEGATE_REL"
printf '# migration table\n' > "$dir/$DELEGATE_TOML"
check "not a git repository: REFUSE" 1 "$(run_gate "$dir")"
rm -rf "$dir"

# Renamed path with the content changed in the same commit. `git log -- <new
# path>` reports one commit and `--follow` does not bridge it, so the walk sees
# a single generation and nothing looks wrong -- except that the table records
# hashes the walk cannot account for.
repo=$(mktemp -d)
mkdir -p "$repo/old"
printf 'v1' > "$repo/old/site_delegate.wasm"
printf '# migration table\n' > "$repo/$DELEGATE_TOML"
git_init "$repo"
git -C "$repo" add -A
git -C "$repo" commit -q -m initial
record "$repo" "$DELEGATE_TOML" "v1"
mkdir -p "$repo/ui/public/contracts"
git -C "$repo" mv old/site_delegate.wasm "$DELEGATE_REL"
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" add -A
git -C "$repo" commit -q -m "relocate + change"
check "path relocated, recorded hash no longer in history: REFUSE" 1 "$(run_gate "$repo")"
rm -rf "$repo"

# A corrupt object store must not read as a clean history. The predecessor is
# deliberately NOT recorded here, so this isolates the blob-read path: if an
# unreadable object is treated as "the file did not exist at that commit", the
# generation vanishes from the walk entirely and the gate returns 0. Recording
# it first would let the converse rule catch the same mutation and mask this.
repo=$(make_repo "v1")
blob=$(git -C "$repo" rev-parse "HEAD:$DELEGATE_REL")
printf 'v2' > "$repo/$DELEGATE_REL"
git -C "$repo" commit -q -am "to v2"
rm -f "$repo/.git/objects/${blob:0:2}/${blob:2}"
if [ -n "$(git -C "$repo" cat-file blob "$blob" 2>&1 >/dev/null)" ]; then
    check "unreadable blob in history: REFUSE" 1 "$(run_gate "$repo")"
else
    echo "ok   - (skipped: blob still readable, likely packed)"; PASS=$((PASS + 1))
fi
rm -rf "$repo"

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
