#!/usr/bin/env bash
# See the Driver Manager > Tools install of `tablex-mcp` end to end WITHOUT a release: builds the
# server, writes a local manifest that lists it under a file:// URL, and starts the debug app on
# a throwaway data directory with the Developer policy (the host accepts an unsigned file://
# manifest and a file:// artifact only then; a release app never does).
#
# Usage: scripts/run-local-tool-smoke.sh [--no-launch] [--verify]
#   (default)    build, write the manifest, start the app (`cargo run -p tablex-gpui`)
#   --no-launch  build and write the manifest, print the environment to start the app yourself
#   --verify     after you installed it in the app: check the file at the stable path, run the
#                installed `--version` and `--print-config`, to compare the latter with what
#                "Copy client configuration" must have produced
#
# In the app: Driver Manager > Tools > Install. Run the script again with another
# TABLEX_DEV_VERSION (default: <crate version>-dev.<8 hex of the binary's sha256>, so a rebuild is
# an update) and "Check for updates" to see Update; the path stays the same.
#
# Environment:
#   TOOL_PROFILE=dev|dist   cargo profile of the server (default dev: seconds, not minutes)
#   TOOL_BINARY=<file>      use this executable instead of building one
#   TABLEX_DEV_VERSION=<v>  force the version (letters, digits . _ - only, at most 32)
#   SMOKE_DIR=<dir>         scratch directory (default <repo>/target/tool-smoke); holds data/ (the
#                           app's TABLEX_DATA_DIR), drivers/ and the manifest. Delete it to start over.
#
# Nothing here signs or publishes anything, and the app never runs the downloaded file: only
# --verify does, and only on the copy you installed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SMOKE_DIR="${SMOKE_DIR:-${ROOT}/target/tool-smoke}"
MANIFEST="${SMOKE_DIR}/manifest/drivers-manifest.json"
DATA_DIR="${SMOKE_DIR}/data"
DRIVER_DIR="${SMOKE_DIR}/drivers"
MODE="launch"
case "${1:-}" in
    "") ;;
    --no-launch) MODE="print" ;;
    --verify) MODE="verify" ;;
    *) echo "Usage: $0 [--no-launch] [--verify]" >&2; exit 2 ;;
esac

# Where the app installs it: <TABLEX_DATA_DIR>/tablex/tools/tablex-mcp/tablex-mcp[.exe]
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; *) EXE="" ;; esac
INSTALLED="${DATA_DIR}/tablex/tools/tablex-mcp/tablex-mcp${EXE}"

if [[ "${MODE}" == "verify" ]]; then
    if [[ ! -f "${INSTALLED}" ]]; then
        echo "Not installed yet: ${INSTALLED}" >&2
        exit 1
    fi
    echo "Installed: ${INSTALLED} ($(wc -c < "${INSTALLED}" | tr -d ' ') bytes)"
    [[ -x "${INSTALLED}" ]] || { echo "NOT executable" >&2; exit 1; }
    echo "Record:    $(tr -d '\n ' < "$(dirname "${INSTALLED}")/installed.json")"
    echo "Version:   $("${INSTALLED}" --version)"
    echo "--print-config (the object \"Copy client configuration\" copies):"
    "${INSTALLED}" --print-config
    exit 0
fi

PROFILE="${TOOL_PROFILE:-dev}"
if [[ -n "${TOOL_BINARY:-}" ]]; then
    BINARY="${TOOL_BINARY}"
else
    if [[ "${PROFILE}" == "dev" ]]; then
        (cd "${ROOT}" && cargo build -p tablex-mcp)
        BINARY="${ROOT}/target/debug/tablex-mcp${EXE}"
    else
        (cd "${ROOT}" && cargo build --profile "${PROFILE}" -p tablex-mcp)
        BINARY="${ROOT}/target/${PROFILE}/tablex-mcp${EXE}"
    fi
fi
[[ -f "${BINARY}" ]] || { echo "Missing ${BINARY}" >&2; exit 1; }

# Target id of this machine (the same naming as the drivers: macos-aarch64, linux-x86_64, ...).
case "$(uname -s)" in Darwin) OS=macos ;; MINGW*|MSYS*|CYGWIN*) OS=windows ;; *) OS=linux ;; esac
case "$(uname -m)" in arm64|aarch64) ARCH=aarch64 ;; x86_64|amd64) ARCH=x86_64 ;; *) ARCH="$(uname -m)" ;; esac
TARGET="${OS}-${ARCH}"

CRATE_VERSION="$(awk -F'"' '/^\[package\]/ {p=1; next} /^\[/ {p=0} p && /^version[[:space:]]*=/ {print $2; exit}' "${ROOT}/apps/tablex-mcp/Cargo.toml")"
SHA="$(python3 - "${BINARY}" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
)"
VERSION="${TABLEX_DEV_VERSION:-${CRATE_VERSION}-dev.${SHA:0:8}}"

mkdir -p "${SMOKE_DIR}/manifest" "${SMOKE_DIR}/artifacts" "${DATA_DIR}" "${DRIVER_DIR}"
ARTIFACT="${SMOKE_DIR}/artifacts/tablex-mcp-${VERSION}-${TARGET}${EXE}"
cp "${BINARY}" "${ARTIFACT}"
python3 "${ROOT}/scripts/driver_manifest.py" tool-entry \
    --binary "${ARTIFACT}" --version "${VERSION}" --target "${TARGET}" \
    --url "file://${ARTIFACT}" --out "${SMOKE_DIR}/manifest/tablex-mcp-tool-entry.json"
# Not --release: file:// URLs are allowed in a dev manifest. Rebuilt from scratch each time and
# never older than the one it replaces.
python3 "${ROOT}/scripts/driver_manifest.py" merge --out "${MANIFEST}" \
    "${SMOKE_DIR}/manifest/tablex-mcp-tool-entry.json"

ENVIRONMENT=(
    "TABLEX_DRIVER_DEV=1"                       # Developer policy (debug builds only)
    "TABLEX_DRIVER_MANIFEST_URL=file://${MANIFEST}"
    "TABLEX_DATA_DIR=${DATA_DIR}"               # the tool installs under <this>/tablex/tools/
    "TABLEX_DRIVER_DATA_DIR=${DRIVER_DIR}"      # keep the real driver store out of it
)
echo
echo "tablex-mcp ${VERSION} (${TARGET}) is listed in ${MANIFEST}"
echo "After Install, the server is at: ${INSTALLED}"
echo
if [[ "${MODE}" == "print" ]]; then
    echo "Start the app with:"
    echo "  env ${ENVIRONMENT[*]} cargo run -p tablex-gpui"
    echo "then check it with: $0 --verify"
    exit 0
fi
echo "Starting the debug app (Driver Manager > Tools > Install; then run: $0 --verify)"
cd "${ROOT}"
exec env "${ENVIRONMENT[@]}" cargo run -p tablex-gpui
