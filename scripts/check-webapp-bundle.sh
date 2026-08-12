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
#   REACHABLE - referenced, transitively, from index.html. The real graph is
#               index.html -> js -> wasm -> favicon (the favicon's hashed name
#               is embedded in the wasm, not the html), so every file is scanned
#               as text and no naming scheme is hardcoded.
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

# Candidates for the reachability scan: everything that is not a REQUIRED
# fixed-name file.
CANDIDATES=()
for f in "${ALL[@]}"; do
    skip=""
    for r in "${REQUIRED[@]}"; do [ "$f" = "$r" ] && { skip=1; break; }; done
    [ -n "$skip" ] || CANDIDATES+=("$f")
done
[ "${#CANDIDATES[@]}" -gt 0 ] || fail "archive contains no app assets -- the build produced nothing"

# Transitive reachability from index.html. Files are scanned as text (grep -a)
# because the loader names its wasm and the wasm embeds the hashed favicon name.
# A candidate counts as referenced if either its full path or its basename
# appears in a file already reached.
declare -A REACHED=()
FRONTIER=("index.html")
while [ "${#FRONTIER[@]}" -gt 0 ]; do
    current="${FRONTIER[0]}"
    FRONTIER=("${FRONTIER[@]:1}")
    for f in "${CANDIDATES[@]}"; do
        [ -n "${REACHED[$f]:-}" ] && continue
        if grep -aqF -- "$f" "$current" || grep -aqF -- "${f##*/}" "$current"; then
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
