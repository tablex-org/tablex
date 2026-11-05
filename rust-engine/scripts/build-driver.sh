#!/usr/bin/env bash
# Build one driver as an ABI v2 dylib that dyld accepts.
#
# Usage: build-driver.sh <driver-id|crate> [--out-dir DIR]
#   ids: mongodb postgresql mysql mariadb redis sqlite clickhouse
# Prints the final dylib path on stdout (everything else goes to stderr).
#
# Env:
#   TABLEX_DRIVER_STRIP   x (default) | none
#   TABLEX_DRIVER_TARGET  rust target triple (default: host). Cross-built
#                         dylibs cannot be dlopen-verified without Rosetta.
#   CARGO                 cargo binary
#
# Strip strategy. Xcode 27 ld (27037) + cargo's default release
# `strip = "debuginfo"` makes cargo/rustc post-process the image and leaves a
# 4-byte-aligned LINKEDIT string pool that dyld rejects ("mis-aligned LINKEDIT
# string pool"; seen with dbengine-redis). So the build itself never strips
# (CARGO_PROFILE_RELEASE_STRIP=none) and the final shrink is Apple's own
# `strip -x` (drops local symbols, keeps every exported dbengine_* symbol) plus
# an ad-hoc re-sign (strip invalidates the linker's signature). Packaging
# re-verifies the result by dlopen through the real host, so a bad strip fails
# loudly. TABLEX_DRIVER_STRIP=none skips `strip -x` (larger, still loads).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_ENGINE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

NAME=""
OUT_DIR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) [[ -z "${NAME}" ]] && NAME="$1" || { echo "unexpected argument: $1" >&2; exit 2; }; shift ;;
    esac
done
if [[ -z "${NAME}" ]]; then
    echo "Usage: $0 <driver-id|crate> [--out-dir DIR]" >&2
    exit 2
fi
resolve_driver "${NAME}"

STRIP_MODE="${TABLEX_DRIVER_STRIP:-x}"
case "${STRIP_MODE}" in x|none) ;; *) echo "TABLEX_DRIVER_STRIP must be x or none" >&2; exit 2 ;; esac
TARGET="${TABLEX_DRIVER_TARGET:-}"
CARGO_BIN="$(cargo_bin)"
TARGET_DIR="${CARGO_TARGET_DIR:-${RUST_ENGINE_DIR}/target}"

TARGET_ARGS=()
BUILT_DIR="${TARGET_DIR}/release"
ARCH_LABEL="$(host_arch)"
if [[ -n "${TARGET}" ]]; then
    TARGET_ARGS=(--target "${TARGET}")
    BUILT_DIR="${TARGET_DIR}/${TARGET}/release"
    case "${TARGET}" in aarch64-*) ARCH_LABEL=arm64 ;; x86_64-*) ARCH_LABEL=x86_64 ;; esac
fi
OUT_DIR="${OUT_DIR:-${TARGET_DIR}/driver-dylibs/${ARCH_LABEL}}"

echo "==> building ${DRIVER_ID} (${CRATE} ${CARGO_FEATURE_ARGS:-<default features>}) [${ARCH_LABEL}, strip=${STRIP_MODE}]" >&2
# shellcheck disable=SC2086  # CARGO_FEATURE_ARGS is intentionally word-split
CARGO_PROFILE_RELEASE_STRIP=none "${CARGO_BIN}" build --release --locked \
    --manifest-path "${RUST_ENGINE_DIR}/Cargo.toml" -p "${CRATE}" \
    ${CARGO_FEATURE_ARGS} ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} >&2

DYLIB_NAME="$(library_file_name "${CRATE}" "${TARGET}")"
BUILT="${BUILT_DIR}/${DYLIB_NAME}"
[[ -f "${BUILT}" ]] || { echo "build did not produce ${BUILT}" >&2; exit 1; }
mkdir -p "${OUT_DIR}"
OUT="${OUT_DIR}/${DYLIB_NAME}"
cp -f "${BUILT}" "${OUT}"
BEFORE=$(wc -c < "${OUT}" | tr -d ' ')

# Apple's `strip -x` + ad hoc re-sign are macOS-only; ELF / PE builds keep the
# linker output (cargo's own release profile already drops debuginfo there).
if [[ "${STRIP_MODE}" == "x" && "${DYLIB_NAME}" == *.dylib ]]; then
    strip -x "${OUT}"
    codesign --force --sign - "${OUT}" >&2
fi
AFTER=$(wc -c < "${OUT}" | tr -d ' ')
echo "==> ${DRIVER_ID}: ${BEFORE} bytes unstripped -> ${AFTER} bytes (strip=${STRIP_MODE}): ${OUT}" >&2
printf '%s\n' "${OUT}"
