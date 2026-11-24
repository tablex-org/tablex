#!/usr/bin/env bash
# Load a driver dylib through the real host loader (engine_driver_host::DriverLoader:
# dlopen, ABI check, capability parse, symbol resolution, init) and print the
# result. Exits non-zero on any failure.
#
# Usage: verify-driver.sh <dylib> [--json OUT.json]
#   stdout: one summary line; with --json the full inspect JSON is written to OUT.json
#   (the input for scripts/driver_manifest.py).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_ENGINE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
# shellcheck source=driver-registry.sh
source "${SCRIPT_DIR}/driver-registry.sh"

DYLIB=""
JSON_OUT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --json) JSON_OUT="$2"; shift 2 ;;
        *) DYLIB="$1"; shift ;;
    esac
done
[[ -f "${DYLIB}" ]] || { echo "Usage: $0 <dylib> [--json OUT.json]" >&2; exit 2; }

CARGO_BIN="$(cargo_bin)"
# Not named TMP: that is an environment variable on Windows (Git bash exports it), and
# the file named here would become link.exe's temp directory (LNK1104 in the build below).
INSPECT_OUT="$(mktemp -t tablex-inspect.XXXXXX)"
trap 'rm -f "${INSPECT_OUT}"' EXIT

if ! "${CARGO_BIN}" run -q --release --locked --manifest-path "${RUST_ENGINE_DIR}/Cargo.toml" \
    -p engine-driver-host --example inspect-driver -- "${DYLIB}" > "${INSPECT_OUT}"; then
    echo "VERIFY FAILED: host could not load ${DYLIB}" >&2
    exit 1
fi
# Windows runners have `python`, not always `python3`.
"$(command -v python3 || command -v python)" - "${INSPECT_OUT}" "${DYLIB}" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["abi_version"] == 2, f"ABI {d['abi_version']} != 2"
print(f"OK {sys.argv[2]}: driver={d['driver']} abi={d['abi_version']} "
      f"features={d['feature_count']} schemes={','.join(d['uri_schemes'])}")
PY
[[ -z "${JSON_OUT}" ]] || cp "${INSPECT_OUT}" "${JSON_OUT}"
