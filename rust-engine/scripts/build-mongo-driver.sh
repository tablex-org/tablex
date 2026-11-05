#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK="${TABLEX_DRIVER_SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"

# Xcode 27's linker emits a 4-byte-aligned LINKEDIT string pool for this driver,
# which dyld rejects when the image declares SDK 27. Use the installed 26.5 SDK
# without changing the deployment target, selected Xcode, linker, or signing.
if [[ -z "${TABLEX_DRIVER_SDKROOT:-}" && "$(xcrun --sdk macosx --show-sdk-version)" == 27.* ]]; then
    SDK="$(DEVELOPER_DIR=/Library/Developer/CommandLineTools xcrun --sdk macosx26.5 --show-sdk-path)" || {
        echo "Mongo driver build requires macOS SDK 26.5 until the Xcode 27 LINKEDIT issue is resolved." >&2
        exit 1
    }
fi

SDKROOT="${SDK}" "${CARGO:-cargo}" build \
    --manifest-path "${SCRIPT_DIR}/../Cargo.toml" \
    -p dbengine-mongo --release --features abi-v2 "$@"
