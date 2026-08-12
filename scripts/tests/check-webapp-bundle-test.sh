#!/bin/bash
# Tests for the bundle gate (scripts/check-webapp-bundle.sh).
#
# Same shape and same reasoning as check-migration-test.sh: prove the gate can
# REFUSE, and prove it is WIRED IN. A gate that runs on the publish path but
# returns 0 for every input is worse than no gate, because the docs cite it as
# protection.
#
#   WIRING   (source scrapes) - the gate runs inside publish-delta, AFTER the
#            tar that produces the archive and BEFORE sign/publish, under a
#            shell that aborts on its non-zero exit. delta#46 was a wiring bug,
#            not a logic bug: the gate existed and was correct, but ran beside
#            the publish path instead of on it. The specific trap here is that
#            preflight runs as a DEPENDENCY of publish-delta, i.e. before the
#            tar exists, so a bundle gate placed there would inspect a stale or
#            absent archive and pass -- exactly delta#46 again.
#
#   END-TO-END - run the REAL script against synthetic archives. These exercise
#            the whole script, so deleting a check from it fails here.
#
# No cargo build, no dx build: about a second, so it gates every PR.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/../check-webapp-bundle.sh"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0

# Fixtures are built under one scratch root so a mid-suite failure cannot leak
# temp trees.
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

check() { # $1 description, $2 expected exit, $3 actual exit
    if [ "$2" = "$3" ]; then
        echo "ok   - $1 (exit $3)"
        PASS=$((PASS + 1))
    else
        echo "FAIL - $1: expected exit $2, got $3"
        FAIL=$((FAIL + 1))
    fi
}

assert() { check "$1" 0 "$2"; }

# ---------------------------------------------------------------- WIRING ----

makefile_task() { # $1 task name -> that task's block
    # Ends the block on a TOML section header specifically, not on any line
    # starting with "[". bundle-webapp's own repo-root guard begins with
    # `[ -f "$ROOT/Makefile.toml" ]`, and a looser match truncates the body
    # there -- silently hiding every line after it, including the tar and the
    # gate call this file exists to check.
    awk -v t="[tasks.$1]" '$0==t{f=1;next} /^\[[A-Za-z][A-Za-z0-9._-]*\]$/{f=0} f' "$REPO/Makefile.toml"
}

echo "--- wiring ---"

bundle_body=$(makefile_task bundle-webapp)
# EVERY assertion about what the task DOES must read the comment-stripped body.
# A commented-out gate call is the single most likely way this gets disabled --
# someone silencing it "just for one publish" -- and it left all 23 assertions
# green until this was fixed.
bundle_code=$(grep -vE '^\s*#' <<< "$bundle_body")

grep -q './scripts/check-webapp-bundle.sh' <<< "$bundle_code"
assert "bundle-webapp delegates to the gate script" $?

# One implementation. Any reachability logic in the Makefile means a second copy
# is back, which is how delta#46 started. The task's comments legitimately
# discuss index.html and the asset names while implementing none of it.
! grep -qE 'delta-ui_bg|index\.html|unreachable' <<< "$bundle_code"
assert "bundle-webapp contains no inline reimplementation of the gate" $?

# THE delta#46 GUARD: the gate inspects the archive, so it must come after the
# tar that writes it. Compare line numbers within the task body.
tar_line=$(grep -n 'tar -cJf' <<< "$bundle_code" | head -1 | cut -d: -f1)
gate_line=$(grep -n './scripts/check-webapp-bundle.sh' <<< "$bundle_code" | head -1 | cut -d: -f1)
[ -n "$tar_line" ] && [ -n "$gate_line" ] && [ "$gate_line" -gt "$tar_line" ]
assert "the gate runs AFTER the tar step, not before it" $?

# A gate whose non-zero exit is not fatal is decoration. `set -e` is necessary
# but NOT sufficient: `|| true` on the call defeats it while leaving `set -e`
# present, and is exactly what someone reaches for to silence a gate. Both
# halves are checked.
grep -qE '^set -e|^set -euo pipefail' <<< "$bundle_code"
assert "bundle-webapp aborts on a failing command (set -e)" $?

! grep -E './scripts/check-webapp-bundle.sh' <<< "$bundle_code" \
    | grep -qE '\|\|\s*(true|:|exit 0)|;\s*true\s*$'
assert "the gate call is not suffixed with || true / || : / || exit 0" $?

# The gate must not be in preflight: preflight is a DEPENDENCY of publish-delta
# and so runs before the archive exists. This is the trap that produced delta#46
# in the migration gate, and it is available here too.
! grep -q 'check-webapp-bundle' <<< "$(makefile_task preflight)"
assert "the bundle gate is NOT in preflight (it would inspect a stale archive)" $?

publish_body=$(makefile_task publish-delta)

grep -q 'dependencies = .*"bundle-webapp"' <<< "$publish_body"
assert "publish-delta depends on bundle-webapp" $?

# bundle-webapp produces a ready-to-SIGN archive. Before it was split out, the
# only route to one was through publish-delta, so check-migration and the test
# suite had necessarily run first. Extracting it for testability must not open a
# gate-free second route to a signable artifact.
grep -q 'dependencies = .*"preflight"' <<< "$bundle_body"
assert "bundle-webapp depends on preflight (no gate-free route to a signable archive)" $?

# Bundling lives in exactly one task. A second tar in publish-delta would
# rebuild the archive after the gate had already approved a different one.
! grep -q 'tar -cJf' <<< "$publish_body"
assert "publish-delta does not re-tar after the gate has run" $?

grep -q 'fdev publish' <<< "$publish_body"
assert "publish-delta is still the task that publishes" $?

# Depending on the gate is not the same as being stopped by it. cargo-make's
# `ignore_errors`/`force` make a task's non-zero exit non-fatal, and `disabled`
# is worse still: it drops the task from the execution plan entirely, so
# `disabled = true` on bundle-webapp leaves publish-delta signing and publishing
# whatever stale webapp.tar.xz happens to be on disk. That was verified against
# a planted 3-copy archive: cargo make exited 0 and both sign and publish ran.
# Any of the three on any task in this chain would let a REFUSING gate be walked
# straight past while every other assertion here stayed green.
chain_honours_failure=0
for t in bundle-webapp test-bundle-gate preflight publish-delta; do
    if grep -qE '^(ignore_errors|force|disabled) *= *true' <<< "$(makefile_task "$t")"; then
        echo "  [$t] sets ignore_errors/force/disabled, so a failing gate would not stop it" >&2
        chain_honours_failure=1
    fi
done
assert "no task in the publish chain is disabled or ignores a failing dependency" $chain_honours_failure

grep -q 'dependencies = .*"test-bundle-gate"' <<< "$(makefile_task preflight)"
assert "preflight depends on test-bundle-gate (this file)" $?

grep -q 'script = "\./scripts/tests/check-webapp-bundle-test\.sh"' <<< "$(makefile_task test-bundle-gate)"
assert "test-bundle-gate delegates to this file" $?

ci="$REPO/.github/workflows/ci.yml"
grep -q 'run: \./scripts/tests/check-webapp-bundle-test\.sh' "$ci"
assert "CI runs this test file" $?

# ------------------------------------------------------------ END-TO-END ----
# Build synthetic archives and run the REAL gate against them. Names mirror
# dx's real output shape; the gate does not depend on the naming.

JS_NAME="delta-ui-dxhaaaaaaaaaaaaaaaa.js"
WASM_NAME="delta-ui_bg-dxhbbbbbbbbbbbbbbbb.wasm"
FAVICON_NAME="favicon-dxhcccccccccccccccc.svg"

make_bundle() { # echoes an archive path for a well-formed bundle
    local dir archive
    dir=$(mktemp -d -p "$SCRATCH")
    mkdir -p "$dir/src/assets" "$dir/src/contracts"
    # index.html -> js -> wasm -> favicon, matching the REAL reference shapes,
    # which were read off an actual bundle. Getting these wrong is not a
    # cosmetic issue: an earlier version of this fixture omitted the loader's
    # unhashed "delta-ui_bg.wasm" literal, and the staged-wasm case below then
    # passed here while the real gate let a 2MB orphan through. The fixture has
    # to be able to produce the fault, or the case that pins it is theatre.
    #
    #   index.html -> loader   by FULL PATH   ("/./assets/<js>")
    #   loader     -> wasm     by FULL PATH   ("/./assets/<wasm>")
    #                          AND carries wasm-bindgen's own unhashed
    #                          "delta-ui_bg.wasm" literal
    #   wasm       -> favicon  by BARE BASENAME, no path at all
    printf '<html><script type="module" src="/./assets/%s"></script></html>\n' "$JS_NAME" \
        > "$dir/src/index.html"
    printf 'let w="delta-ui_bg.wasm";export default function init(){return fetch("/./assets/%s");}\n' \
        "$WASM_NAME" > "$dir/src/assets/$JS_NAME"
    printf '\0asm\1\0\0\0 some wasm bytes referencing %s\n' "$FAVICON_NAME" \
        > "$dir/src/assets/$WASM_NAME"
    printf '<svg/>\n' > "$dir/src/assets/$FAVICON_NAME"
    printf 'contract\n' > "$dir/src/contracts/site_contract.wasm"
    printf 'delegate\n' > "$dir/src/contracts/site_delegate.wasm"
    printf 'body{}\n' > "$dir/src/styles.css"
    printf 'body{}\n' > "$dir/src/main.css"
    archive="$dir/webapp.tar.xz"
    tar -cJf "$archive" -C "$dir/src" .
    echo "$archive"
}

# Rebuild the archive after a fixture has mutated the extracted tree.
retar() { # $1 archive
    local dir="$(dirname "$1")"
    rm -f "$1"
    tar -cJf "$1" -C "$dir/src" .
}

run_gate() { bash "$GATE" "$1" >/dev/null 2>&1; echo $?; }

# Assert the REASON, not just the exit code. Several refusals are reachable by
# more than one route -- an empty assets/ trips both the empty-directory check
# and, further down, "index.html references no js" -- so a test that only reads
# the exit code cannot tell which check fired, and stays green when one of them
# is deleted. Mutation testing found exactly that: removing the empty-assets
# refusal left every exit-code assertion here passing. Pinning the message means
# each case pins one specific check.
check_refusal() { # $1 description, $2 archive, $3 expected message fragment
    local out code
    out=$(bash "$GATE" "$2" 2>&1); code=$?
    if [ "$code" -ne 1 ]; then
        echo "FAIL - $1: expected exit 1, got $code"
        FAIL=$((FAIL + 1))
    elif ! grep -qF -- "$3" <<< "$out"; then
        echo "FAIL - $1: refused, but not for the expected reason"
        echo "         wanted message containing: $3"
        echo "         got: $(head -1 <<< "$out")"
        FAIL=$((FAIL + 1))
    else
        echo "ok   - $1 (exit 1, correct reason)"
        PASS=$((PASS + 1))
    fi
}

echo "--- end-to-end (runs the real gate) ---"

a=$(make_bundle)
check "well-formed bundle: proceed" 0 "$(run_gate "$a")"
rm -rf "$(dirname "$a")"

# THE BUG. A second build's wasm + loader left behind by the missing clean.
a=$(make_bundle)
d="$(dirname "$a")/src"
cp "$d/assets/$WASM_NAME" "$d/assets/delta-ui_bg-dxh5741e00000000.wasm"
cp "$d/assets/$JS_NAME"   "$d/assets/delta-ui-dxh5741e00000000.js"
retar "$a"
check_refusal "orphaned wasm + loader from an earlier build: REFUSE" "$a" "unreachable from index.html"
rm -rf "$(dirname "$a")"

# A lone orphan, which a "the chain resolves" check alone would miss.
a=$(make_bundle)
d="$(dirname "$a")/src"
cp "$d/assets/$WASM_NAME" "$d/assets/delta-ui_bg-dxh5741e00000000.wasm"
retar "$a"
check_refusal "single orphaned wasm: REFUSE" "$a" "unreachable from index.html"
rm -rf "$(dirname "$a")"

# Outside assets/. An assets-only scan returned "Bundle OK" for all three of
# these. The staged-wasm case is the one that actually happens: dx writes the
# unhashed wasm-bindgen output to public/wasm/ before moving it into assets/, so
# an interrupted build leaves a full-size copy there.
# THE MOTIVATING CASE, and the one that regressed once already. dx stages the
# UNHASHED wasm-bindgen output at wasm/delta-ui_bg.wasm, and the loader contains
# the literal "delta-ui_bg.wasm" (wasm-bindgen's own default name), so a
# basename-anywhere matcher laundered this 2MB orphan into "reachable" and the
# gate printed Bundle OK. Caught by hash-shape, which cannot be argued out of a
# refusal by what some other file happens to contain.
a=$(make_bundle)
d="$(dirname "$a")/src"
mkdir -p "$d/wasm"
cp "$d/assets/$WASM_NAME" "$d/wasm/delta-ui_bg.wasm"
retar "$a"
check_refusal "stale staged copy at wasm/delta-ui_bg.wasm: REFUSE" "$a" "carry no dx content hash"
rm -rf "$(dirname "$a")"

# The same laundering, in the SAME directory as the reference. Same-directory
# basename matching is deliberately still allowed (the wasm names the favicon
# with no path), so reachability alone would pass this. Only hash-shape refuses.
a=$(make_bundle)
d="$(dirname "$a")/src"
cp "$d/assets/$WASM_NAME" "$d/assets/delta-ui_bg.wasm"
retar "$a"
check_refusal "unhashed stray beside the loader that names it: REFUSE" "$a" "carry no dx content hash"
rm -rf "$(dirname "$a")"

# The converse: a HASHED copy in another directory, which hash-shape passes.
# This is what pins the path-resolution rule specifically -- its basename is
# identical to the genuinely-reachable wasm, so a basename-anywhere matcher
# marks it reached, and only resolving the reference relative to the referrer's
# directory (assets/, not wasm/) refuses it.
a=$(make_bundle)
d="$(dirname "$a")/src"
mkdir -p "$d/wasm"
cp "$d/assets/$WASM_NAME" "$d/wasm/$WASM_NAME"
retar "$a"
check_refusal "hashed copy in another directory sharing the loader's target basename: REFUSE" \
    "$a" "unreachable from index.html"
rm -rf "$(dirname "$a")"

a=$(make_bundle)
d="$(dirname "$a")/src"
printf 'orphan\n' > "$d/orphan.js"
retar "$a"
check_refusal "unhashed orphan at the archive root: REFUSE" "$a" "carry no dx content hash"
rm -rf "$(dirname "$a")"

a=$(make_bundle)
d="$(dirname "$a")/src"
mkdir -p "$d/assets/snippets"
printf 'stale\n' > "$d/assets/snippets/stale.js"
retar "$a"
check_refusal "unhashed orphan nested in assets/snippets/: REFUSE" "$a" "carry no dx content hash"
rm -rf "$(dirname "$a")"

# The destructive clean before the build makes a missing runtime file a real
# possibility, and contracts/*.wasm cannot be covered by reachability: their
# names appear nowhere in the bundle, because the app builds the path at run
# time. Checked by presence instead.
for required in contracts/site_contract.wasm contracts/site_delegate.wasm styles.css main.css; do
    a=$(make_bundle)
    d="$(dirname "$a")/src"
    rm "$d/$required"
    retar "$a"
    check_refusal "required file $required missing: REFUSE" "$a" "missing 1 file(s) the app needs"
    rm -rf "$(dirname "$a")"
done

# Generality: not a delta-ui file at all. A name-scoped check would miss this.
a=$(make_bundle)
d="$(dirname "$a")/src"
cp "$d/assets/$FAVICON_NAME" "$d/assets/favicon-dxh5741e00000000.svg"
retar "$a"
check_refusal "orphaned favicon (not a delta-ui name): REFUSE" "$a" "unreachable from index.html"
rm -rf "$(dirname "$a")"

# Stale index.html pointing at an old loader while the fresh pair is orphaned.
a=$(make_bundle)
d="$(dirname "$a")/src"
cp "$d/assets/$JS_NAME" "$d/assets/delta-ui-dxhf6e5b00000000.js"
cp "$d/assets/$WASM_NAME" "$d/assets/delta-ui_bg-dxhf6e5b00000000.wasm"
retar "$a"
check_refusal "fresh pair present but index.html still points at the old one: REFUSE" "$a" "unreachable from index.html"
rm -rf "$(dirname "$a")"

# Dangling references: the chain must resolve INSIDE the archive.
#
# This fixture isolates the entry-point check deliberately. Simply deleting the
# loader would leave the wasm and favicon orphaned, so the UNREACHABLE check
# fires first and this case would pin nothing new. Here every asset present is
# reachable, and the only thing wrong is that none of them is an entry point.
a=$(make_bundle)
d="$(dirname "$a")/src"
rm -f "$d"/assets/*
printf '<svg/>\n' > "$d/assets/$FAVICON_NAME"
printf '<html><link rel="icon" href="/assets/%s"></html>\n' "$FAVICON_NAME" > "$d/index.html"
retar "$a"
check_refusal "assets present and reachable but no js entry point: REFUSE" "$a" "references no js in the bundle"
rm -rf "$(dirname "$a")"

a=$(make_bundle)
d="$(dirname "$a")/src"
rm "$d/assets/$WASM_NAME" "$d/assets/$FAVICON_NAME"
retar "$a"
check_refusal "loader references a wasm that is not in the bundle: REFUSE" "$a" "references no wasm in the bundle"
rm -rf "$(dirname "$a")"

# VACUITY GUARD. "No unreachable assets" is trivially true of an empty
# assets/ directory, so an app-less bundle must be refused on its own account.
a=$(make_bundle)
d="$(dirname "$a")/src"
rm -f "$d"/assets/*
retar "$a"
check_refusal "no app assets at all (build produced nothing): REFUSE" "$a" "contains no app assets"
rm -rf "$(dirname "$a")"

a=$(make_bundle)
d="$(dirname "$a")/src"
rm "$d/index.html"
retar "$a"
check_refusal "no index.html: REFUSE" "$a" "missing 1 file(s) the app needs"
rm -rf "$(dirname "$a")"

# Fail closed: no archive at all is a refusal, never a skip.
check_refusal "archive does not exist: REFUSE" "/nonexistent/webapp.tar.xz" "no such archive"

check "no archive argument: REFUSE" 1 "$(bash "$GATE" >/dev/null 2>&1; echo $?)"

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
