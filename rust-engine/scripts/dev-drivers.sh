#!/usr/bin/env bash
# One command: rebuild + verify + repackage drivers for local testing and write
# dist/drivers/drivers-manifest.json (what debug GPUI builds read).
#
# Usage: rust-engine/scripts/dev-drivers.sh [driver-id ...]   (default: all seven + the plugins)
#   ids: mongodb postgresql mysql mariadb redis sqlite clickhouse
#        mongo-shell  (the MongoDB Shell plugin, an add-on of the mongodb driver: build-plugin.sh
#                      then package-plugin.sh with an ad hoc signature; it requires the mongodb
#                      driver version of this checkout, so build `mongodb` too)
#
# Each driver: build-driver.sh (ABI v2 features, strip policy) -> verify-driver.sh
# (real host dlopen) -> package-driver.sh (unsigned local, TEAM_ID LOCAL_UNSIGNED;
# entry generated from the dylib's capabilities) -> merged manifest, validated by
# the host's manifest parser + installer.
#
# Versions are "<crate version>-dev.<first 8 hex of dylib sha256>" so every rebuild
# shows up as an update in the Driver Manager instead of "Installed".
# TABLEX_DEV_VERSION=<v> forces a fixed version for all drivers.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_ENGINE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ROOT="$(cd "${RUST_ENGINE_DIR}/.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

if [[ $# -gt 0 ]]; then IDS="$*"; else IDS="${ALL_DRIVER_IDS} mongo-shell sqruff"; fi

if [[ $# -eq 0 ]]; then
    # A full rebuild owns the output directory: drop entries from earlier runs.
    rm -f "${ROOT}"/dist/drivers/*-manifest-entry.json "${ROOT}"/dist/drivers/*-tool-entry.json
fi

for id in ${IDS}; do
    if [[ "${id}" == "mongo-shell" || "${id}" == "sqruff" ]]; then
        continue  # packaged below, after the drivers
    fi
    resolve_driver "${id}"
    DYLIB="$(bash "${SCRIPT_DIR}/build-driver.sh" "${DRIVER_ID}")"
    VERSION="${TABLEX_DEV_VERSION:-$(crate_version "${RUST_ENGINE_DIR}" "${CRATE}")-dev.$(shasum -a 256 "${DYLIB}" | cut -c1-8)}"
    (cd "${ROOT}" && ALLOW_UNSIGNED_LOCAL=1 bash "${SCRIPT_DIR}/package-driver.sh" "${DRIVER_ID}" "${VERSION}" "${DYLIB}")
done

for id in ${IDS}; do
    if [[ "${id}" == "mongo-shell" ]]; then
        EXE="$(bash "${SCRIPT_DIR}/build-plugin.sh")"
        VERSION="${TABLEX_DEV_VERSION:-$(crate_version "${RUST_ENGINE_DIR}" tablex-mongo-shell)-dev.$(shasum -a 256 "${EXE}" | cut -c1-8)}"
        (cd "${ROOT}" && ALLOW_UNSIGNED_LOCAL=1 bash "${SCRIPT_DIR}/package-plugin.sh" "${VERSION}" "${EXE}")
    fi
done

for id in ${IDS}; do
    if [[ "${id}" == "sqruff" ]]; then
        EXE="$(bash "${SCRIPT_DIR}/build-plugin.sh" --crate tablex-sqruff)"
        VERSION="${TABLEX_DEV_VERSION:-$(crate_version "${RUST_ENGINE_DIR}" tablex-sqruff)-dev.$(shasum -a 256 "${EXE}" | cut -c1-8)}"
        (cd "${ROOT}" && ALLOW_UNSIGNED_LOCAL=1 bash "${SCRIPT_DIR}/package-plugin.sh" --tool tablex-sqruff "${VERSION}" "${EXE}")
    fi
done

bash "${SCRIPT_DIR}/make-local-driver-manifest.sh"
echo
echo "Debug app: manifest at ${ROOT}/dist/drivers/drivers-manifest.json (TABLEX_DRIVER_MANIFEST_URL overrides)."
echo "Use a throwaway TABLEX_DRIVER_DATA_DIR to avoid mixing with installed drivers."
