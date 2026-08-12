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
# The general property, not a count: EVERY file under assets/ must be reachable
# from index.html by following references transitively (index.html -> js -> wasm
# -> favicon; the favicon's hashed name is embedded in the wasm). Reachability
# subsumes "exactly one of each" and also catches shapes nobody enumerated -- a
# lone orphaned wasm, an orphaned favicon, a stale index.html pointing at an old
# loader while the fresh pair sits unreferenced -- while still permitting a
# future dx that legitimately emits several reachable chunks. A hardcoded
# "exactly 1" would fail that last case and would miss the orphaned favicon.
#
# Reachability alone is vacuously true of an EMPTY assets/ directory, so the
# chain is also required to resolve: index.html must reach a js, and that js
# must reach a wasm. An empty or app-less bundle is refused, not passed.
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

[ -f index.html ] || fail "archive has no index.html"
[ -d assets ] || fail "archive has no assets/ directory"

# All bundled assets, by basename.
mapfile -t ASSETS < <(find assets -maxdepth 1 -type f -printf '%f\n' | sort)
[ "${#ASSETS[@]}" -gt 0 ] || fail "archive has an empty assets/ directory -- the build produced no app"

# Transitive reachability from index.html. Every file is scanned as text
# (grep -a): the loader names its wasm, and the wasm embeds the hashed favicon
# name, so the whole graph is discoverable without knowing dx's naming scheme.
declare -A REACHED=()
FRONTIER=("index.html")
while [ "${#FRONTIER[@]}" -gt 0 ]; do
    current="${FRONTIER[0]}"
    FRONTIER=("${FRONTIER[@]:1}")
    for asset in "${ASSETS[@]}"; do
        [ -n "${REACHED[$asset]:-}" ] && continue
        if grep -aqF -- "$asset" "$current"; then
            REACHED[$asset]=1
            FRONTIER+=("assets/$asset")
        fi
    done
done

# 1. Nothing unreferenced. This is the check that fails on a bloated bundle.
UNREACHED=()
for asset in "${ASSETS[@]}"; do
    [ -n "${REACHED[$asset]:-}" ] || UNREACHED+=("$asset")
done
if [ "${#UNREACHED[@]}" -gt 0 ]; then
    echo "FAILED: ${#UNREACHED[@]} asset(s) in the bundle are unreachable from index.html:"
    for asset in "${UNREACHED[@]}"; do
        echo "          assets/$asset ($(stat -c%s "assets/$asset") bytes)"
    done
    echo "        These are stale copies from earlier builds. Every published byte"
    echo "        should be reachable; see scripts/check-webapp-bundle.sh and delta#70."
    exit 1
fi

# 2. The chain actually resolves, so an empty or app-less bundle cannot pass
#    check 1 vacuously.
JS_REF=""
for asset in "${ASSETS[@]}"; do
    case "$asset" in *.js) if grep -aqF -- "$asset" index.html; then JS_REF="$asset"; break; fi ;; esac
done
[ -n "$JS_REF" ] || fail "index.html references no js in assets/ -- the bundle has no entry point"

WASM_REF=""
for asset in "${ASSETS[@]}"; do
    case "$asset" in *.wasm) if grep -aqF -- "$asset" "assets/$JS_REF"; then WASM_REF="$asset"; break; fi ;; esac
done
[ -n "$WASM_REF" ] || fail "assets/$JS_REF references no wasm in assets/ -- the loader has nothing to load"

# 3. Staleness guard. If the build output this archive was made from is still on
#    disk, the archive must describe it. Catches a gate that is inspecting a
#    tarball from an earlier build -- the delta#46 shape, where a gate ran but
#    not against the thing being published. Skipped when the tree is absent
#    (CI, or checking an archive fetched from elsewhere).
BUILD_ASSETS="$(dirname "$ARCHIVE")/../dx/delta-ui/release/web/public/assets"
if [ -d "$BUILD_ASSETS" ]; then
    on_disk="$(find "$BUILD_ASSETS" -maxdepth 1 -type f -printf '%f\n' | sort)"
    in_archive="$(printf '%s\n' "${ASSETS[@]}")"
    [ "$on_disk" = "$in_archive" ] || fail \
        "archive assets/ does not match the build output at $BUILD_ASSETS -- this archive is stale"
fi

echo "Bundle OK: index.html -> assets/$JS_REF -> assets/$WASM_REF"
echo "  ${#ASSETS[@]} asset(s), all reachable from index.html"
echo "  archive:   $(du -h "$ARCHIVE" | cut -f1) compressed"
echo "  extracted: $(du -sh . | cut -f1)"
