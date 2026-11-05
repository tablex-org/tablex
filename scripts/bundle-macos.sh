#!/usr/bin/env bash
# Assemble target/release/bundle/TableX.app from a release build of tablex-gpui.
#
# Without a bundle, macOS names the app after the executable ("tablex-gpui") and
# gives it no Dock icon identity; the menu bar title comes from CFBundleName.
#
# Usage: scripts/bundle-macos.sh [--no-build] [--binary PATH]
#   --no-build     reuse an existing binary instead of running cargo build
#   --binary PATH  executable to bundle (default: target/release/tablex-gpui)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-$root/target}"
binary="$target_dir/release/tablex-gpui"
build=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build=0 ;;
        --binary) shift; binary="${1:?--binary needs a path}"; build=0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

[ "$(uname -s)" = "Darwin" ] || { echo "bundle-macos.sh only runs on macOS" >&2; exit 1; }

if [ "$build" = 1 ]; then
    cargo build --release -p tablex-gpui
fi
[ -x "$binary" ] || { echo "missing executable: $binary" >&2; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"/\1/p' apps/tablex-gpui/Cargo.toml | head -n 1)"
# The same `org.tablex` prefix as the signed tools (org.tablex.tablex-mcp, ...).
bundle_id="${TABLEX_BUNDLE_ID:-org.tablex.TableX}"
app="$target_dir/release/bundle/TableX.app"
contents="$app/Contents"

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$binary" "$contents/MacOS/TableX"

# Icon: apps/tablex-gpui/assets/AppIcon.png (square, >= 1024px) -> AppIcon.icns.
# TODO: no app icon artwork exists in the repo yet; the bundle is built without
# one until that file is added.
icon_key=""
icon_png="apps/tablex-gpui/assets/AppIcon.png"
if [ -f "$icon_png" ] && command -v iconutil >/dev/null && command -v sips >/dev/null; then
    iconset="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$icon_png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        sips -z "$((size * 2))" "$((size * 2))" "$icon_png" \
            --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$contents/Resources/AppIcon.icns"
    rm -rf "$(dirname "$iconset")"
    icon_key="    <key>CFBundleIconFile</key><string>AppIcon</string>"
else
    echo "note: $icon_png not found, bundling without an icon" >&2
fi

# Third-party licence texts (Apache-2.0 for the code adapted from dbx, section 4 of the licence):
# they ship with the binary, so they go inside the bundle before it is signed.
[ -f licenses/Apache-2.0.txt ] || { echo "missing licenses/Apache-2.0.txt" >&2; exit 1; }
cp -R licenses "$contents/Resources/licenses"

cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>TableX</string>
    <key>CFBundleDisplayName</key><string>TableX</string>
    <key>CFBundleIdentifier</key><string>$bundle_id</string>
    <key>CFBundleExecutable</key><string>TableX</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
$icon_key
</dict>
</plist>
PLIST
plutil -lint "$contents/Info.plist" >/dev/null

# Ad-hoc signature: enough to launch locally; distribution needs a Developer ID.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

echo "$app"
