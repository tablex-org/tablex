#!/usr/bin/env bash
# Set up a Debian 11 (bullseye) container to build the Linux release files (release-app.yml and
# release-drivers.yml run their Linux build jobs in `debian:11@sha256:...`). Debian 11 has glibc 2.31:
# a binary built here needs 2.31 at most, so it runs on Debian 11+, Ubuntu 20.04+ and RHEL 9. Runs as
# root in the container, after the checkout.
#
# What it installs: the compilers and the -dev packages a build links against (the windowing, font and
# Vulkan libraries are opened at run time from the user's system, so a binary does not carry them), a
# pinned rustup with the stable toolchain, and a static OpenSSL (scripts/build-static-openssl.sh, which
# says why). It then puts the toolchain on the PATH of the later steps ($GITHUB_PATH / $GITHUB_ENV).
#
# Usage: scripts/linux-container-setup.sh [extra apt packages...]
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
packages=(
    build-essential pkg-config cmake perl python3 curl ca-certificates git
    clang libclang-dev                       # bindgen (mysql_common)
    libfontconfig1-dev libfreetype6-dev      # GPUI text
    libxkbcommon-dev libxkbcommon-x11-dev    # keyboard
    libwayland-dev                           # Wayland
    libx11-dev libx11-xcb-dev libxcb1-dev libxcb-xkb-dev   # X11
    libvulkan-dev                            # rendering (wgpu)
    file                                     # appimagetool checks the binary type with it
)
# Bullseye's support has ended: the main mirrors still list its security updates but no longer
# serve them (404), so every source is the archive, one consistent, signed snapshot.
sed -i -E 's#https?://(deb|security)\.debian\.org#http://archive.debian.org#; /bullseye-updates/d' \
    /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true
apt-get -o Acquire::Check-Valid-Until=false update
apt-get install -y --no-install-recommends "${packages[@]}" "$@"

# rustup, pinned to a release whose installer is checked against a known hash; the toolchain is `stable`,
# as in the other workflows.
case "$(uname -m)" in
    x86_64) triple=x86_64-unknown-linux-gnu; rustup_sha=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55 ;;
    aarch64) triple=aarch64-unknown-linux-gnu; rustup_sha=9f608c142e0e38a78957d0661ba1dfaf0b6fb37184cfaa4b9a0aa57af6184282 ;;
    *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
curl -fsSL --retry 3 -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/1.29.3/${triple}/rustup-init"
echo "${rustup_sha}  /tmp/rustup-init" | sha256sum -c -
chmod +x /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain stable
export PATH="${CARGO_HOME}/bin:${PATH}"
rustc --version
ldd --version | head -n 1

"$(dirname "${BASH_SOURCE[0]}")/build-static-openssl.sh" /opt/openssl

if [ -n "${GITHUB_ENV:-}" ]; then
    {
        echo "RUSTUP_HOME=${RUSTUP_HOME}"
        echo "CARGO_HOME=${CARGO_HOME}"
        echo "OPENSSL_DIR=/opt/openssl"
        echo "OPENSSL_STATIC=1"
    } >> "${GITHUB_ENV}"
    echo "${CARGO_HOME}/bin" >> "${GITHUB_PATH}"
fi
