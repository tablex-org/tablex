#!/usr/bin/env bash
# Pack a Linux build of tablex into the two release assets:
#   tablex-<version>-linux-<arch>.tar.gz     bin/tablex, bin/tablex-mcp, share/ (desktop file, icons), licences
#   tablex-<version>-linux-<arch>.AppImage   the same tablex binary behind AppRun, nothing else bundled
#
# The AppImage is a thin wrapper: no glibc, no Vulkan / GL / Mesa, no X11 / Wayland libraries. They
# come from the host, as they do for the tarball.
#
# Usage: scripts/package-linux.sh --version X.Y.Z --arch x86_64|aarch64 --out DIR
#            [--binary PATH] [--mcp PATH] [--max-glibc 2.31] [--appimagetool PATH]
#   --binary   release build of tablex-gpui (default: target/dist/tablex-gpui)
#   --mcp      release build of tablex-mcp (default: target/dist/tablex-mcp)
#   --max-glibc  fail when the binary needs a newer glibc symbol version than this
#   --appimagetool  an appimagetool to use (default: the pinned release is downloaded and checked)
#                   The AppImage runtime is always the pinned release, downloaded and checked.
# Needs: objdump, readelf, ImageMagick (magick or convert), desktop-file-validate (optional), tar, curl.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="" arch="" out="" max_glibc="" appimagetool=""
binary="target/dist/tablex-gpui"
mcp="target/dist/tablex-mcp"
while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="${2:?}"; shift ;;
        --arch) arch="${2:?}"; shift ;;
        --out) out="${2:?}"; shift ;;
        --binary) binary="${2:?}"; shift ;;
        --mcp) mcp="${2:?}"; shift ;;
        --max-glibc) max_glibc="${2:?}"; shift ;;
        --appimagetool) appimagetool="${2:?}"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
[ -n "$version" ] && [ -n "$arch" ] && [ -n "$out" ] || { echo "usage: $0 --version V --arch ARCH --out DIR" >&2; exit 2; }
case "$arch" in
    x86_64) machine="Advanced Micro Devices X86-64"; tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
        runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
    aarch64) machine="AArch64"; tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
        runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
    *) echo "unsupported --arch $arch (x86_64 or aarch64)" >&2; exit 2 ;;
esac
tool_version="1.9.1"
# The runtime is the part of an AppImage that starts it on the user's machine. appimagetool 1.9.1 fetches
# it from the moving `continuous` release of AppImage/type2-runtime when it is not given one, so it is
# pinned here (a fixed release tag and a SHA-256 per architecture) and passed with --runtime-file.
runtime_version="20251108"
for file in "$binary" "$mcp"; do
    [ -x "$file" ] || { echo "missing executable: $file" >&2; exit 1; }
done

# --- what the binary needs from the host ----------------------------------------------------------------
for file in "$binary" "$mcp"; do
    readelf -h "$file" | grep -q "Machine:.*$machine" || { echo "$file is not a $arch executable" >&2; exit 1; }
    needed="$(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')"
    glibc="$(objdump -T "$file" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)"
    echo "$(basename "$file"): needs glibc ${glibc:-?}; shared libraries: $needed"
    # OpenSSL is linked statically (scripts/build-static-openssl.sh); a soname here means a build that
    # only runs where that exact version is installed.
    case "$needed" in *libssl*|*libcrypto*) echo "$file links libssl / libcrypto dynamically" >&2; exit 1 ;; esac
    if [ -n "$max_glibc" ] && [ -n "$glibc" ] && [ "$(printf '%s\n%s\n' "$glibc" "$max_glibc" | sort -V | tail -n 1)" != "$max_glibc" ]; then
        echo "$file needs glibc $glibc, more than $max_glibc" >&2
        exit 1
    fi
done

# --- icons and desktop file -----------------------------------------------------------------------------
if command -v magick >/dev/null; then resize() { magick "$1" -resize "$2x$2" "$3"; }
elif command -v convert >/dev/null; then resize() { convert "$1" -resize "$2x$2" "$3"; }
else echo "ImageMagick (magick or convert) is needed to scale the icon" >&2; exit 1; fi
icon_png="apps/tablex-gpui/assets/AppIcon.png"
[ -f "$icon_png" ] || { echo "missing $icon_png" >&2; exit 1; }
[ -f licenses/Apache-2.0.txt ] || { echo "missing licenses/Apache-2.0.txt" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The window's app id (apps/tablex-gpui/src/app/shell.rs) is `tablex`: StartupWMClass and the file
# name tie the window to this entry and its icon.
desktop="$work/tablex.desktop"
cat > "$desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=tablex
GenericName=Database Client
Comment=A fast native database client
Exec=tablex
Icon=tablex
Terminal=false
Categories=Development;Database;
StartupWMClass=tablex
DESKTOP
if command -v desktop-file-validate >/dev/null; then desktop-file-validate "$desktop"; fi

# <prefix>/share/icons/hicolor/<size>x<size>/apps/tablex.png
icons="$work/icons"
for size in 16 32 48 64 128 256 512; do
    mkdir -p "$icons/hicolor/${size}x${size}/apps"
    resize "$icon_png" "$size" "$icons/hicolor/${size}x${size}/apps/tablex.png"
done

# --- the tarball ----------------------------------------------------------------------------------------
name="tablex-${version}-linux-${arch}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
stage="$work/$name"
mkdir -p "$stage/bin" "$stage/share/applications" "$stage/share/icons"
cp "$binary" "$stage/bin/tablex"
cp "$mcp" "$stage/bin/tablex-mcp"
cp "$desktop" "$stage/share/applications/tablex.desktop"
cp -R "$icons/hicolor" "$stage/share/icons/"
cp LICENSE "$stage/LICENSE"
cp -R licenses "$stage/licenses"
cat > "$stage/README.txt" <<README
tablex ${version} for Linux (${arch})

Run it from here:        ./bin/tablex
Install for your user:   cp -R bin share ~/.local/    (put ~/.local/bin on your PATH)
                         then tablex is in the application menu, with its icon.

tablex-mcp is the MCP server that lets an AI assistant use your saved connections; the app can
also install and update it from Driver Manager. Database drivers are installed from the app
(Driver Manager); they are not in this archive.

It needs a graphical session (Wayland or X11) with a Vulkan driver, and glibc 2.31 or newer
(Debian 11, Ubuntu 20.04, RHEL 9 and later). Data is kept in ~/.local/share/tablex.

Licence: AGPL-3.0 (LICENSE); third-party texts are in licenses/.
README
tar -C "$work" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"

# --- the AppImage ---------------------------------------------------------------------------------------
appdir="$work/tablex.AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons"
cp "$binary" "$appdir/usr/bin/tablex"
cp "$desktop" "$appdir/tablex.desktop"
cp "$desktop" "$appdir/usr/share/applications/tablex.desktop"
cp -R "$icons/hicolor" "$appdir/usr/share/icons/"
cp "$icons/hicolor/256x256/apps/tablex.png" "$appdir/tablex.png"
cp "$icons/hicolor/256x256/apps/tablex.png" "$appdir/.DirIcon"
mkdir -p "$appdir/usr/share/licenses/tablex"
cp LICENSE "$appdir/usr/share/licenses/tablex/LICENSE"
cp -R licenses/. "$appdir/usr/share/licenses/tablex/"
ln -s usr/bin/tablex "$appdir/AppRun"

if [ -z "$appimagetool" ]; then
    appimagetool="$work/appimagetool"
    curl -fsSL --retry 3 -o "$appimagetool" \
        "https://github.com/AppImage/appimagetool/releases/download/${tool_version}/appimagetool-${arch}.AppImage"
    echo "${tool_sha}  ${appimagetool}" | sha256sum -c -
    chmod +x "$appimagetool"
fi
runtime="$work/runtime-$arch"
curl -fsSL --retry 3 -o "$runtime" \
    "https://github.com/AppImage/type2-runtime/releases/download/${runtime_version}/runtime-${arch}"
echo "${runtime_sha}  ${runtime}" | sha256sum -c -
# appimagetool is an AppImage itself: run it unpacked, so a runner without FUSE can start it.
ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" --no-appstream --runtime-file "$runtime" "$appdir" "$out/$name.AppImage"

echo "$out/$name.tar.gz"
echo "$out/$name.AppImage"
