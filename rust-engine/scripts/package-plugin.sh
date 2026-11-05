#!/usr/bin/env bash
# Package a plugin executable and emit its manifest tool entry: the MongoDB Shell plugin (the
# default) or the SQL formatter tool (`--tool tablex-sqruff`).
#
# The plugin is one bare executable per target (no archive), downloaded by the Driver Manager
# from the signed manifest and installed at <app data>/tools/<tool>/. The MongoDB Shell's entry
# says it is an add-on of the MongoDB driver (`requires`): the app installs it only next to a
# driver at least MIN_DRIVER_VERSION and offers to install that driver first. The formatter
# needs no driver; its entry carries its licence and source repository (driver_manifest.py).
#
# Usage:
#   SIGNING_IDENTITY="Developer ID Application: Name (TEAMID)" \
#   ./rust-engine/scripts/package-plugin.sh [--tool <id>] [--target <id>] <version> <path-to-executable>
#   (get the executable from rust-engine/scripts/build-plugin.sh)
#
#   --tool <id>    tablex-mongo-shell (default) | tablex-sqruff
#   --target <id>  manifest target of the executable (default: this host). The protocol smoke
#                  runs only when it is this host's.
#
# Output:
#   dist/tools/<tool>-<target>[.exe]
#   dist/drivers/<tool>-<target>-tool-entry.json   (`driver_manifest.py merge` folds it in)
# Env:
#   PACKAGE_TOOLS_DIR     where the executable goes (default: dist/tools)
#   PACKAGE_OUT_DIR       where the entry goes (default: dist/drivers)
#   ARTIFACT_BASE_URL     https URL prefix of the executable (default: file:// URL, development only)
#   MIN_DRIVER_VERSION    oldest MongoDB driver that has `mongo.command` (default: the dbengine-mongo
#                         crate version of this checkout: bump it in the commit that adds the op)
#   PLUGIN_SKIP_SMOKE     1 = do not run the executable (the release job: it holds an unlocked signing
#                         keychain, and the build job already smoke-tested this very file's build)
#   ALLOW_UNSIGNED_LOCAL  1 = ad hoc signature on macOS instead of SIGNING_IDENTITY (local only)
#   TABLEX_AD_HOC_SIGNING 1 = the same for a release made without a Developer ID
#   SIGNING_IDENTITY      macOS Developer ID identity (hardened runtime, fixed identifier)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

TARGET=""
TOOL="tablex-mongo-shell"
POSITIONAL=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --tool) TOOL="${2:-}"; shift 2 || { echo "--tool needs a value" >&2; exit 2; } ;;
        --target) TARGET="${2:-}"; shift 2 || { echo "--target needs a value" >&2; exit 2; } ;;
        *) POSITIONAL+=("$1"); shift ;;
    esac
done
VERSION="${POSITIONAL[0]:-}"
BINARY="${POSITIONAL[1]:-}"
if [[ -z "${VERSION}" || -z "${BINARY}" || ${#POSITIONAL[@]} -gt 2 ]]; then
    echo "Usage: $0 [--tool <id>] [--target <id>] <version> <path-to-executable>" >&2
    exit 1
fi
[[ -f "${BINARY}" ]] || { echo "Missing executable: ${BINARY}" >&2; exit 1; }
case "${TOOL}" in tablex-mongo-shell|tablex-sqruff) ;; *) echo "Unknown --tool '${TOOL}'" >&2; exit 2 ;; esac

HOST_TARGET="$(host_target)"
TARGET="${TARGET:-${HOST_TARGET}}"
case "${TARGET}" in
    macos-aarch64|macos-x86_64|linux-x86_64|linux-aarch64|windows-x86_64|windows-aarch64) ;;
    *) echo "Unknown --target '${TARGET}'" >&2; exit 2 ;;
esac
TARGET_OS="$(target_os "${TARGET}")"
NAME="${TOOL}-${TARGET}"
[[ "${TARGET_OS}" == "windows" ]] && NAME="${NAME}.exe"

TOOLS_DIR="${PACKAGE_TOOLS_DIR:-${ROOT}/dist/tools}"
OUT_DIR="${PACKAGE_OUT_DIR:-${ROOT}/dist/drivers}"
mkdir -p "${TOOLS_DIR}" "${OUT_DIR}"
ARTIFACT="${TOOLS_DIR}/${NAME}"
cp -f "${BINARY}" "${ARTIFACT}"
chmod 755 "${ARTIFACT}"

# Signed BEFORE the entry is made: the SHA-256 in the signed manifest is the file users download.
if [[ "${TARGET_OS}" == "macos" ]]; then
    if [[ -n "${SIGNING_IDENTITY:-}" ]]; then
        codesign --force --options runtime --timestamp --identifier "org.tablex.${TOOL}" \
            --sign "${SIGNING_IDENTITY}" "${ARTIFACT}"
        codesign --verify --strict "${ARTIFACT}"
    elif [[ "${ALLOW_UNSIGNED_LOCAL:-0}" == "1" || "${TABLEX_AD_HOC_SIGNING:-0}" == "1" ]]; then
        codesign --force --identifier "org.tablex.${TOOL}" --sign - "${ARTIFACT}"
        codesign --verify --strict "${ARTIFACT}"
    else
        echo "The plugin is not signed. Set SIGNING_IDENTITY (or ALLOW_UNSIGNED_LOCAL=1 for a local build, TABLEX_AD_HOC_SIGNING=1 for a release without a Developer ID)." >&2
        exit 1
    fi
fi

# The protocol smoke: our own just-built executable says hello (first frame, 4-byte length +
# JSON) and exits when its stdin ends; it runs no script, opens no connection, needs no network.
if [[ "${TARGET}" == "${HOST_TARGET}" && "${PLUGIN_SKIP_SMOKE:-0}" != "1" ]]; then
    HELLO="$("$(cd "${TOOLS_DIR}" && pwd)/${NAME}" < /dev/null 2>/dev/null | head -c 64 | tr -d '\000-\010\013-\037' || true)"
    case "${HELLO}" in
        *'"hello"'*'"protocol"'*) ;;
        *) echo "${ARTIFACT} did not say hello (got: ${HELLO})" >&2; exit 1 ;;
    esac
fi

REQUIRES=()
if [[ "${TOOL}" == "tablex-mongo-shell" ]]; then
    MIN_DRIVER_VERSION="${MIN_DRIVER_VERSION:-$(crate_version "${ROOT}/rust-engine" dbengine-mongo)}"
    REQUIRES=(--requires-driver "mongodb:${MIN_DRIVER_VERSION}")
fi
if [[ -n "${ARTIFACT_BASE_URL:-}" ]]; then
    URL="${ARTIFACT_BASE_URL%/}/${NAME}"
else
    URL="$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).resolve().as_uri())' "${ARTIFACT}")"
fi
ENTRY="${OUT_DIR}/${TOOL}-${TARGET}-tool-entry.json"
python3 "${ROOT}/scripts/driver_manifest.py" tool-entry \
    --binary "${ARTIFACT}" --version "${VERSION}" --target "${TARGET}" --url "${URL}" \
    --id "${TOOL}" ${REQUIRES[@]+"${REQUIRES[@]}"} --out "${ENTRY}"

echo "Packaged: ${ARTIFACT}"
echo "Tool entry: ${ENTRY}${REQUIRES[@]+" (needs the mongodb driver >= ${MIN_DRIVER_VERSION})"}"
echo "SHA-256: $(shasum -a 256 "${ARTIFACT}" | awk '{print $1}')"
echo "Size: $(wc -c < "${ARTIFACT}" | tr -d ' ')"
