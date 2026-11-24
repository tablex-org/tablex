#!/usr/bin/env bash
# Build OpenSSL as static, position-independent libraries into PREFIX (Linux release builds).
#
# The app and the MySQL / MariaDB / PostgreSQL drivers reach OpenSSL through native-tls. Linked
# dynamically, a binary built on an old distro (the Debian 11 container of release-app.yml) asks for
# libssl.so.1.1, which Debian 12 / Ubuntu 22.04 / Fedora no longer ship, and one built on a new
# distro asks for a soname the old ones lack. Linked statically it needs none, so the same file
# runs on both. cargo then finds it through `OPENSSL_DIR=PREFIX OPENSSL_STATIC=1`.
#
# Usage: scripts/build-static-openssl.sh PREFIX   (needs perl, make, a C compiler, curl, sha256sum)
set -euo pipefail

version="3.5.9"
sha256="603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a"

prefix="${1:?usage: $0 PREFIX}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

curl -fsSL --retry 3 -o "${work}/openssl.tar.gz" \
    "https://github.com/openssl/openssl/releases/download/openssl-${version}/openssl-${version}.tar.gz"
echo "${sha256}  ${work}/openssl.tar.gz" | sha256sum -c -
tar -xzf "${work}/openssl.tar.gz" -C "${work}"
cd "${work}/openssl-${version}"
# OpenSSL's make is noisy: keep its output for a failure only.
{
    ./Configure --prefix="${prefix}" --libdir=lib no-shared no-tests no-docs -fPIC &&
        make -j"$(nproc)" build_libs &&
        make install_dev
} >"${work}/build.log" 2>&1 || { cat "${work}/build.log" >&2; exit 1; }
ls "${prefix}/lib/libssl.a" "${prefix}/lib/libcrypto.a"
