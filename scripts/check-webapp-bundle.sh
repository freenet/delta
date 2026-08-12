#!/bin/bash
# Publish gate: refuse a webapp archive that is not self-consistent.
#
# WHY THIS EXISTS
#
# dx writes CONTENT-HASHED asset filenames, so a changed build lands beside its
# predecessor rather than replacing it. Nothing pruned the old one and the
# publish task tarred the whole directory, so the bundle was append-only across
# builds: it grew a full ~2MB wasm + js pair on every publish, indefinitely, and
# the live bundle carried three builds' worth (delta#70).
#
# The wasted bytes are the smaller cost. With several wasms in the bundle, "did
# my fix ship?" can no longer be answered by grepping the bundle, because a grep
# for a new symbol hits a stale copy just as readily as the live one. Checking
# reachability here is what keeps grep-the-bundle an honest deploy check.
#
# WHAT IT ASSERTS
#
# Every file in the archive is accounted for, in one of exactly two ways:
#
#   REQUIRED  - a fixed-name file the app needs at runtime. These cannot be
#               covered by reachability: contracts/*.wasm are fetched via a path
#               the app builds at run time, so their names appear nowhere in the
#               bundle. Their absence is checked directly instead, which matters
#               because the publish task now wipes the whole build directory
#               before building.
#
#   ACCOUNTED - carries a dx content hash in its name (check 0) AND is
#               referenced, transitively, from index.html (check 1). The real
#               graph is index.html -> js -> wasm -> favicon; the favicon's
#               hashed name is embedded in the wasm, not the html, so every file
#               is scanned as text.
#
# Both are needed and neither is redundant: hash-shape catches an unhashed stray
# whatever any file happens to contain, and reachability catches a HASHED orphan
# from an earlier build, which is the original delta#70 bug and which hash-shape
# cannot distinguish from the live one.
#
# Reachability is the general property, deliberately not a count: it subsumes
# "exactly one of each", and also catches an orphaned favicon, a stale
# index.html pointing at an old loader while the fresh pair sits unreferenced, a
# root-level orphan, a nested assets/snippets/ leftover, and dx's staged
# public/wasm/delta-ui_bg.wasm. A hardcoded "exactly 1" would miss all of those
# and would also fail a future dx that legitimately emits several reachable
# chunks. The scan covers the WHOLE archive rather than assets/ alone, so the
# publish task's "everything outside assets/ has a fixed name" claim is enforced
# here rather than merely asserted in a comment.
#
# Reachability is vacuously true of an archive with no assets, so the chain is
# required to resolve as well: index.html must reach a js, and that js must
# reach a wasm.
set -euo pipefail

ARCHIVE="${1:?usage: check-webapp-bundle.sh <webapp.tar.xz>}"
[ -f "$ARCHIVE" ] || { echo "FAILED: no such archive: $ARCHIVE"; exit 1; }
# Absolute: the checks below run from inside a temp extraction dir.
ARCHIVE="$(cd "$(dirname "$ARCHIVE")" && pwd)/$(basename "$ARCHIVE")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
tar -xJf "$ARCHIVE" -C "$WORK"
cd "$WORK"

fail() { echo "FAILED: $*"; exit 1; }

# Fixed-name files the app needs. index.html is first because everything else
# is judged relative to it.
REQUIRED=(
    index.html
    styles.css
    main.css
    contracts/site_contract.wasm
    contracts/site_delegate.wasm
)

MISSING=()
for f in "${REQUIRED[@]}"; do
    [ -f "$f" ] || MISSING+=("$f")
done
if [ "${#MISSING[@]}" -gt 0 ]; then
    echo "FAILED: the archive is missing ${#MISSING[@]} file(s) the app needs:"
    printf '          %s\n' "${MISSING[@]}"
    exit 1
fi

# Every file in the archive, relative to its root.
mapfile -t ALL < <(find . -type f -printf '%P\n' | sort)

dir_of() { case "$1" in */*) printf '%s' "${1%/*}" ;; *) printf '' ;; esac; }

# Candidates for the reachability scan: everything that is not a REQUIRED
# fixed-name file.
CANDIDATES=()
for f in "${ALL[@]}"; do
    skip=""
    for r in "${REQUIRED[@]}"; do [ "$f" = "$r" ] && { skip=1; break; }; done
    [ -n "$skip" ] || CANDIDATES+=("$f")
done
[ "${#CANDIDATES[@]}" -gt 0 ] || fail "archive contains no app assets -- the build produced nothing"

# CHECK 0: every non-required file must carry a dx content hash in its name.
#
# This is the PRIMARY check because it involves no parsing at all, so nothing a
# file happens to contain can talk it out of a refusal. It exists because the
# reachability scan below was fooled exactly once, by exactly this shape: dx
# stages the UNHASHED wasm-bindgen output at wasm/delta-ui_bg.wasm, and the
# loader legitimately contains the literal string "delta-ui_bg.wasm" (it is
# wasm-bindgen's own default name). A 2MB orphan was therefore laundered into
# "reachable" by a substring coincidence with a file that really was reachable.
#
# Hash-shape catches that by construction, and keeps catching it whatever the
# matcher below is later changed to: the staged copy is precisely the file whose
# name is NOT hashed. If dx ever changes its hash format this check starts
# refusing everything, which is loud rather than silent -- the correct direction
# for a publish gate to fail.
UNHASHED=()
for f in "${CANDIDATES[@]}"; do
    [[ "${f##*/}" =~ -dxh[0-9a-f]+\.[A-Za-z0-9]+$ ]] || UNHASHED+=("$f")
done
if [ "${#UNHASHED[@]}" -gt 0 ]; then
    echo "FAILED: ${#UNHASHED[@]} file(s) carry no dx content hash and are not required files:"
    for f in "${UNHASHED[@]}"; do
        echo "          $f ($(stat -c%s "$f") bytes)"
    done
    echo "        dx names every bundled asset <name>-dxh<hash>.<ext>. An unhashed"
    echo "        file here is a stray -- most likely dx's staged wasm/delta-ui_bg.wasm"
    echo "        left by an interrupted build. See delta#70."
    exit 1
fi

# CHECK 1: transitive reachability from index.html, for orphans that DO carry a
# hash (a previous build's asset, which check 0 cannot distinguish).
#
# Files are scanned as text (grep -a) because the loader names its wasm and the
# wasm embeds the hashed favicon name. References resolve RELATIVE TO THE
# REFERRER: a candidate is reached if the referrer contains its full archive
# path, or if the referrer contains its bare basename AND the two live in the
# same directory. Bare-basename matching cannot be dropped -- the wasm names the
# favicon with no path at all -- but restricting it to same-directory means a
# reference living in assets/ can no longer reach a file in wasm/.
declare -A REACHED=()
FRONTIER=("index.html")
while [ "${#FRONTIER[@]}" -gt 0 ]; do
    current="${FRONTIER[0]}"
    FRONTIER=("${FRONTIER[@]:1}")
    current_dir="$(dir_of "$current")"
    for f in "${CANDIDATES[@]}"; do
        [ -n "${REACHED[$f]:-}" ] && continue
        if grep -aqF -- "$f" "$current" \
           || { [ "$(dir_of "$f")" = "$current_dir" ] && grep -aqF -- "${f##*/}" "$current"; }; then
            REACHED[$f]=1
            FRONTIER+=("$f")
        fi
    done
done

# 1. Nothing unreferenced. This is the check that fails on a bloated bundle.
UNREACHED=()
for f in "${CANDIDATES[@]}"; do
    [ -n "${REACHED[$f]:-}" ] || UNREACHED+=("$f")
done
if [ "${#UNREACHED[@]}" -gt 0 ]; then
    echo "FAILED: ${#UNREACHED[@]} file(s) in the bundle are unreachable from index.html:"
    for f in "${UNREACHED[@]}"; do
        echo "          $f ($(stat -c%s "$f") bytes)"
    done
    echo "        These are stale copies from earlier builds. Every published byte"
    echo "        should be reachable; see scripts/check-webapp-bundle.sh and delta#70."
    exit 1
fi

# 2. The chain actually resolves, so an asset-less bundle cannot pass check 1
#    vacuously.
JS_REF=""
for f in "${CANDIDATES[@]}"; do
    case "$f" in *.js) if grep -aqF -- "${f##*/}" index.html; then JS_REF="$f"; break; fi ;; esac
done
[ -n "$JS_REF" ] || fail "index.html references no js in the bundle -- there is no entry point"

WASM_REF=""
for f in "${CANDIDATES[@]}"; do
    case "$f" in *.wasm) if grep -aqF -- "${f##*/}" "$JS_REF"; then WASM_REF="$f"; break; fi ;; esac
done
[ -n "$WASM_REF" ] || fail "$JS_REF references no wasm in the bundle -- the loader has nothing to load"

# 3. Staleness guard. If the build output this archive was made from is still on
#    disk, the archive must describe it. Catches a gate that is inspecting a
#    tarball from an earlier build -- the delta#46 shape, where a gate ran but
#    not against the thing being published. Skipped when the tree is absent
#    (CI, or checking an archive fetched from elsewhere).
BUILD_DIR="$(dirname "$ARCHIVE")/../dx/delta-ui/release/web/public"
if [ -d "$BUILD_DIR" ]; then
    on_disk="$(find "$BUILD_DIR" -type f -printf '%P\n' | sort)"
    in_archive="$(printf '%s\n' "${ALL[@]}")"
    [ "$on_disk" = "$in_archive" ] || fail \
        "archive contents do not match the build output at $BUILD_DIR -- this archive is stale"
fi

echo "Bundle OK: index.html -> $JS_REF -> $WASM_REF"
echo "  ${#ALL[@]} file(s): ${#REQUIRED[@]} required, ${#CANDIDATES[@]} reachable from index.html"
echo "  archive:   $(du -h "$ARCHIVE" | cut -f1) compressed"
echo "  extracted: $(du -sh . | cut -f1)"
