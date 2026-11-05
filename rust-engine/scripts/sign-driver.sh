#!/usr/bin/env bash
set -euo pipefail

DYLIB="${1:-}"
if [[ -z "${DYLIB}" || ! -f "${DYLIB}" ]]; then
    echo "Usage: $0 <path-to-dylib>"
    exit 1
fi

SIGNING_IDENTITY="${SIGNING_IDENTITY:-}"
if [[ -z "${SIGNING_IDENTITY}" ]]; then
    echo "SIGNING_IDENTITY is required"
    exit 1
fi

codesign --force --options runtime --timestamp --sign "${SIGNING_IDENTITY}" "${DYLIB}"
codesign --verify --deep --strict --verbose=2 "${DYLIB}"
shasum -a 256 "${DYLIB}"
