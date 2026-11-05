#!/usr/bin/env bash
# Package one driver library and emit its manifest entry.
#
# The library is loaded through the real host (verify-driver.sh) BEFORE anything
# is written; the manifest entry's id / displayName / uriSchemes / appAbiVersion
# / features are copied from what the library reports, never hand-listed.
#
# Usage:
#   SIGNING_IDENTITY="Developer ID Application: Name (TEAMID)" \
#   TEAM_ID="TEAMID" \
#   ./rust-engine/scripts/package-driver.sh [--target <id>] [--inspect FILE] \
#       mysql 1.0.0 <path-to-abi-v2-library>
#   (get the library from rust-engine/scripts/build-driver.sh <driver>)
#
#   --target <id>   manifest target of the library: macos-aarch64 | macos-x86_64 |
#                   linux-x86_64 | linux-aarch64 | windows-x86_64 | windows-aarch64
#                   (default: this host). Forwarded to driver_manifest.py entry.
#   --inspect FILE  the JSON `verify-driver.sh --json` wrote on a runner of the
#                   target. Required when the target is not this host (a library
#                   cannot be loaded here); then no load check runs in this script.
#
# Output (macOS: zipped and codesigned; Linux / Windows: the bare library):
#   dist/drivers/libdbengine_<id>-<arch>.dylib.zip       (macOS)
#   dist/drivers/libdbengine_<id>-<target>.so            (Linux)
#   dist/drivers/dbengine_<id>-<target>.dll              (Windows)
#   dist/drivers/<id>-<target>-manifest-entry.json   (one per target; `merge` folds them)
# Env:
#   PACKAGE_OUT_DIR       output directory (default: dist/drivers)
#   ARTIFACT_BASE_URL     https URL prefix for the zip (default: file:// URL of dist/drivers)
#   ALLOW_UNSIGNED_LOCAL  1 = accept an ad-hoc/unsigned dylib (teamId "" in the entry; file:// URLs only)
#   TABLEX_AD_HOC_SIGNING 1 = a release without a Developer ID: ad-hoc sign on macOS, teamId ""
#                         (https URLs allowed; driver_manifest.py reads the same variable)
#   SIGNING_IDENTITY / TEAM_ID  as before
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

TARGET=""
INSPECT_GIVEN=""
POSITIONAL=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET="${2:-}"; shift 2 || { echo "--target needs a value" >&2; exit 2; } ;;
        --inspect) INSPECT_GIVEN="${2:-}"; shift 2 || { echo "--inspect needs a value" >&2; exit 2; } ;;
        *) POSITIONAL+=("$1"); shift ;;
    esac
done
DRIVER_ARG="${POSITIONAL[0]:-}"
VERSION="${POSITIONAL[1]:-}"
DYLIB="${POSITIONAL[2]:-}"

if [[ -z "${DRIVER_ARG}" || -z "${VERSION}" || -z "${DYLIB}" || ${#POSITIONAL[@]} -gt 3 ]]; then
    echo "Usage: $0 [--target <id>] [--inspect FILE] <driver-id> <version> <path-to-library>"
    exit 1
fi
resolve_driver "${DRIVER_ARG}"

HOST_TARGET="$(host_target)"
TARGET="${TARGET:-${HOST_TARGET}}"
case "${TARGET}" in
    macos-aarch64|macos-x86_64|linux-x86_64|linux-aarch64|windows-x86_64|windows-aarch64) ;;
    *) echo "Unknown --target '${TARGET}'" >&2; exit 2 ;;
esac
TARGET_OS="$(target_os "${TARGET}")"
if [[ "${TARGET}" != "${HOST_TARGET}" && -z "${INSPECT_GIVEN}" ]]; then
    echo "Cannot load a ${TARGET} library on ${HOST_TARGET}: pass --inspect FILE (verify-driver.sh --json from a ${TARGET} runner)" >&2
    exit 2
fi
if [[ -n "${INSPECT_GIVEN}" && ! -f "${INSPECT_GIVEN}" ]]; then
    echo "Missing inspect file: ${INSPECT_GIVEN}" >&2
    exit 1
fi

if [[ ! -f "${DYLIB}" ]]; then
    echo "Missing dylib: ${DYLIB}"
    exit 1
fi

TEAM_ID="${TEAM_ID:-}"
SIGNING_IDENTITY="${SIGNING_IDENTITY:-}"

# Code signing and the Team ID are macOS only: Linux has no native signature
# layer and Windows drivers are checked by the signed manifest's SHA-256 (plus
# an Authenticode publisher once those are signed).
if [[ "${TARGET_OS}" == "macos" ]]; then
    if [[ -n "${SIGNING_IDENTITY}" ]]; then
        codesign --force --options runtime --timestamp --sign "${SIGNING_IDENTITY}" "${DYLIB}"
    elif [[ "${TABLEX_AD_HOC_SIGNING:-0}" == "1" ]]; then
        # A release without a Developer ID: ad-hoc signed, no Team ID; the
        # signed manifest's SHA-256 is what the host checks.
        codesign --force --sign - "${DYLIB}"
        TEAM_ID=""
    fi

    if [[ -z "${TEAM_ID}" ]]; then
        TEAM_ID=$(codesign -dv --verbose=4 "${DYLIB}" 2>&1 | awk -F= '/TeamIdentifier=/ {print $2; exit}')
    fi

    if [[ -z "${TEAM_ID}" || "${TEAM_ID}" == "not set" ]]; then
        if [[ "${ALLOW_UNSIGNED_LOCAL:-0}" == "1" || "${TABLEX_AD_HOC_SIGNING:-0}" == "1" ]]; then
            TEAM_ID=""   # ad-hoc: no Team ID (Developer policy, or a manifest that pins none)
        else
            echo "Driver is not signed with a Team ID. Set SIGNING_IDENTITY or TEAM_ID (or TABLEX_AD_HOC_SIGNING=1 for a release without a Developer ID)."
            exit 1
        fi
    fi
else
    TEAM_ID=""
fi

# Per-driver metadata the dylib does not carry (licence) and the minimum feature
# set packaging must see in the dylib (guards against shipping a driver built
# without its features). Everything else in the manifest comes from the dylib.
case "${DRIVER_ID}" in
    mongodb)
        LICENSE="Apache-2.0"
        REQUIRED_FEATURES='["navigator", "query.mongo_find", "query.mongo_script", "database.create"]'
        ;;
    sqlite)
        LICENSE="Public-Domain"
        REQUIRED_FEATURES='["navigator", "query.sql", "query.arrow_c_data"]'
        ;;
    postgresql|mysql|mariadb)
        LICENSE="GPL-3.0"
        REQUIRED_FEATURES='["navigator", "query.sql", "query.arrow_c_data"]'
        ;;
    clickhouse)
        LICENSE="Apache-2.0"
        REQUIRED_FEATURES='["navigator", "query.sql", "query.arrow_c_data", "operation.cancel", "metadata.table_properties"]'
        ;;
    redis)
        LICENSE="MIT"
        REQUIRED_FEATURES='["navigator", "query.redis_key", "query.redis_command"]'
        ;;
esac

case "${TARGET}" in
    *-aarch64) ARCH=arm64 ;;
    *) ARCH="${TARGET#*-}" ;;
esac
OUT_DIR="${PACKAGE_OUT_DIR:-${ROOT}/dist/drivers}"
mkdir -p "${OUT_DIR}"

INSPECT_JSON="$(mktemp -t tablex-driver-inspect.XXXXXX)"
trap 'rm -f "${INSPECT_JSON}"' EXIT
if [[ -n "${INSPECT_GIVEN}" ]]; then
    cp "${INSPECT_GIVEN}" "${INSPECT_JSON}"
else
    bash "${SCRIPT_DIR}/verify-driver.sh" "${DYLIB}" --json "${INSPECT_JSON}"
fi

# The manifest id must be the id the dylib reports (the host matches them).
REPORTED_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["driver"])' "${INSPECT_JSON}")
if [[ "${REPORTED_ID}" != "${DRIVER_ID}" ]]; then
    echo "dylib reports driver id '${REPORTED_ID}' but '${DRIVER_ID}' was requested" >&2
    exit 1
fi

case "${TARGET_OS}" in
    macos)
        ARTIFACT="${OUT_DIR}/libdbengine_${DRIVER_ID}-${ARCH}.dylib.zip"
        rm -f "${ARTIFACT}"
        ditto -c -k --sequesterRsrc "${DYLIB}" "${ARTIFACT}"
        ;;
    windows)
        ARTIFACT="${OUT_DIR}/dbengine_${DRIVER_ID}-${TARGET}.dll"
        cp -f "${DYLIB}" "${ARTIFACT}"
        ;;
    *)
        ARTIFACT="${OUT_DIR}/libdbengine_${DRIVER_ID}-${TARGET}.so"
        cp -f "${DYLIB}" "${ARTIFACT}"
        ;;
esac

if [[ -n "${ARTIFACT_BASE_URL:-}" ]]; then
    URL="${ARTIFACT_BASE_URL%/}/$(basename "${ARTIFACT}")"
else
    URL="$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).resolve().as_uri())' "${ARTIFACT}")"
fi

ENTRY="${OUT_DIR}/${DRIVER_ID}-${TARGET}-manifest-entry.json"
python3 "${ROOT}/scripts/driver_manifest.py" entry \
    --inspect "${INSPECT_JSON}" --version "${VERSION}" --zip "${ARTIFACT}" --url "${URL}" \
    --target "${TARGET}" --team-id "${TEAM_ID}" --license "${LICENSE}" \
    --require "${REQUIRED_FEATURES}" --out "${ENTRY}"

echo "Packaged: ${ARTIFACT}"
echo "Manifest entry: ${ENTRY}"
echo "SHA-256: $(shasum -a 256 "${ARTIFACT}" | awk '{print $1}')"
echo "Size: $(wc -c < "${ARTIFACT}" | tr -d ' ')"
