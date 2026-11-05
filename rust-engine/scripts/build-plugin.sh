#!/usr/bin/env bash
# Build a plugin executable: the MongoDB Shell plugin (`tablex-mongo-shell`, the default) or the
# SQL formatter tool (`--crate tablex-sqruff`). One executable the app downloads through the
# Driver Manager and spawns once per script / format (it is never loaded into the app).
#
# Usage: build-plugin.sh [--crate tablex-mongo-shell|tablex-sqruff] [--out-dir DIR]
# Prints the final executable path on stdout (everything else goes to stderr).
#
# Env:
#   TABLEX_PLUGIN_STRIP   x (default) | none
#   TABLEX_PLUGIN_TARGET  rust target triple (default: host)
#   CARGO                 cargo binary
#
# Same strip policy as the drivers (see build-driver.sh): cargo never post-processes the image
# (CARGO_PROFILE_RELEASE_STRIP=none), the shrink is Apple's own `strip -x` plus an ad hoc
# re-sign. ELF / PE builds keep the linker output. QuickJS is compiled by `cc` (rquickjs-sys
# ships pre-generated bindings for the targets in the release matrix: no libclang needed).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_ENGINE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

CRATE="tablex-mongo-shell"
OUT_DIR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --crate)
            CRATE="${2:-}"
            case "${CRATE}" in tablex-mongo-shell|tablex-sqruff) ;; *) echo "--crate must be tablex-mongo-shell or tablex-sqruff" >&2; exit 2 ;; esac
            shift 2 ;;
        -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unexpected argument: $1" >&2; exit 2 ;;
    esac
done

STRIP_MODE="${TABLEX_PLUGIN_STRIP:-x}"
case "${STRIP_MODE}" in x|none) ;; *) echo "TABLEX_PLUGIN_STRIP must be x or none" >&2; exit 2 ;; esac
TARGET="${TABLEX_PLUGIN_TARGET:-}"
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
OUT_DIR="${OUT_DIR:-${TARGET_DIR}/plugins/${ARCH_LABEL}}"

EXE_NAME="${CRATE}"
case "${TARGET:-$(uname -s)}" in *windows*|MINGW*|MSYS*|CYGWIN*) EXE_NAME="${CRATE}.exe" ;; esac

echo "==> building ${CRATE} [${ARCH_LABEL}, strip=${STRIP_MODE}]" >&2
CARGO_PROFILE_RELEASE_STRIP=none "${CARGO_BIN}" build --release --locked \
    --manifest-path "${RUST_ENGINE_DIR}/Cargo.toml" -p "${CRATE}" \
    ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} >&2

BUILT="${BUILT_DIR}/${EXE_NAME}"
[[ -f "${BUILT}" ]] || { echo "build did not produce ${BUILT}" >&2; exit 1; }
mkdir -p "${OUT_DIR}"
OUT="${OUT_DIR}/${EXE_NAME}"
cp -f "${BUILT}" "${OUT}"
chmod 755 "${OUT}"
BEFORE=$(wc -c < "${OUT}" | tr -d ' ')

if [[ "${STRIP_MODE}" == "x" && "$(uname -s)" == "Darwin" && "${EXE_NAME}" != *.exe ]]; then
    strip -x "${OUT}"
    codesign --force --sign - "${OUT}" >&2
fi
AFTER=$(wc -c < "${OUT}" | tr -d ' ')
echo "==> ${CRATE}: ${BEFORE} bytes unstripped -> ${AFTER} bytes (strip=${STRIP_MODE}): ${OUT}" >&2
printf '%s\n' "${OUT}"
