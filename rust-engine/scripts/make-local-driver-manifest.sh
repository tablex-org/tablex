#!/usr/bin/env bash
# Combine packaged driver manifest entries (dist/drivers/*-manifest-entry.json) and tool
# entries (dist/drivers/*-tool-entry.json: the MongoDB Shell plugin) into
# dist/drivers/drivers-manifest.json, then validate the result with the
# host's own manifest parser + installer (inspect-driver --manifest).
#
# Set SKIP_HOST_CHECK=1 to skip the validation step.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

OUT_DIR="${ROOT}/dist/drivers"
mkdir -p "${OUT_DIR}"

ENTRIES=()
for entry in "${OUT_DIR}"/*-manifest-entry.json "${OUT_DIR}"/*-tool-entry.json; do
    [[ -e "${entry}" ]] && ENTRIES+=("${entry}")
done
[[ ${#ENTRIES[@]} -gt 0 ]] || { echo "No *-manifest-entry.json in ${OUT_DIR}; run package-driver.sh first" >&2; exit 1; }

python3 "${ROOT}/scripts/driver_manifest.py" merge --out "${OUT_DIR}/drivers-manifest.json" "${ENTRIES[@]}"

if [[ "${SKIP_HOST_CHECK:-0}" != "1" ]]; then
    "$(cargo_bin)" run -q --release --manifest-path "${ROOT}/rust-engine/Cargo.toml" \
        -p engine-driver-host --example inspect-driver -- --manifest "${OUT_DIR}/drivers-manifest.json" \
        | python3 -c '
import json, sys
r = json.load(sys.stdin)
for d in r["drivers"]:
    print(("ok   " if d["ok"] else "FAIL ") + d["id"], d.get("version", ""), "abi=%s features=%s" % (d.get("abi_version"), d.get("feature_count")), "; ".join(d.get("problems", [])))
sys.exit(0 if r["ok"] else 1)'
fi
