#!/usr/bin/env bash
# Pack a TableX.app (from scripts/bundle-macos.sh) into a compressed disk image
# with an /Applications shortcut, so a user drags the app across to install it.
#
# Usage: scripts/dmg-macos.sh APP OUT.dmg
set -euo pipefail

[ "$(uname -s)" = "Darwin" ] || { echo "dmg-macos.sh only runs on macOS" >&2; exit 1; }
[ $# -eq 2 ] || { echo "usage: $0 APP OUT.dmg" >&2; exit 2; }
app="$1"
out="$2"
[ -d "$app/Contents/MacOS" ] || { echo "not an app bundle: $app" >&2; exit 1; }

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"

mkdir -p "$(dirname "$out")"
rm -f "$out"
hdiutil create -volname "TableX" -srcfolder "$stage" -fs HFS+ -format UDZO -imagekey zlib-level=9 \
    -ov "$out" >/dev/null
hdiutil verify "$out" >/dev/null
echo "$out"
