#!/bin/bash
# Assert that the webapp archive we are about to publish is self-consistent:
# exactly one app wasm + one loader, and index.html's reference chain resolves
# to them from inside the archive.
#
# This exists because the bundle silently accumulated stale copies for months
# (delta#70): dx writes CONTENT-HASHED asset filenames, so a changed build lands
# beside its predecessor rather than replacing it, and the publish task tarred
# the whole directory. The bundle grew ~2MB per publish and every publish
# shipped several builds' worth of the app, only one of which was reachable.
#
# The second cost is what makes this a gate rather than a size nag: with several
# wasms in the bundle, "did my fix ship?" can no longer be answered by grepping
# the bundle, because a grep hits a stale copy just as readily as the live one.
# Checking the chain here keeps grep-the-bundle an honest deploy check.
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

# 1. Exactly one of each app artifact.
js_count=$(find assets -maxdepth 1 -name 'delta-ui-*.js' | wc -l)
wasm_count=$(find assets -maxdepth 1 -name 'delta-ui_bg-*.wasm' | wc -l)
[ "$js_count" -eq 1 ] || fail "bundle contains $js_count delta-ui-*.js loaders, expected 1 (stale copies were not cleaned)"
[ "$wasm_count" -eq 1 ] || fail "bundle contains $wasm_count delta-ui_bg-*.wasm files, expected 1 (stale copies were not cleaned)"

# 2. index.html -> loader, resolved inside the bundle.
js_ref=$(grep -o 'assets/delta-ui-[A-Za-z0-9]*\.js' index.html | sort -u)
[ "$(echo "$js_ref" | wc -l)" -eq 1 ] || fail "index.html references more than one loader: $js_ref"
[ -f "$js_ref" ] || fail "index.html references $js_ref, which is not in the bundle"

# 3. loader -> wasm, resolved inside the bundle.
wasm_ref=$(grep -o 'delta-ui_bg-[A-Za-z0-9]*\.wasm' "$js_ref" | sort -u)
[ "$(echo "$wasm_ref" | wc -l)" -eq 1 ] || fail "$js_ref references more than one wasm: $wasm_ref"
[ -f "assets/$wasm_ref" ] || fail "$js_ref references assets/$wasm_ref, which is not in the bundle"

# 4. The files that exist are the ones the chain points at, not a coincidental pair.
[ -f "$js_ref" ] && [ "$(find assets -maxdepth 1 -name 'delta-ui-*.js')" = "$js_ref" ] \
    || fail "the only loader present is not the one index.html references"

echo "Bundle OK: index.html -> $js_ref -> assets/$wasm_ref"
echo "  archive:   $(du -h "$ARCHIVE" | cut -f1) compressed"
echo "  extracted: $(du -sh . | cut -f1)"
