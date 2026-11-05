#!/usr/bin/env bash
# Sourced by the driver build/package scripts. Single place that knows how each
# driver is built:  id -> crate, cargo feature args, cdylib file name.
#
# ABI v2 is what the GPUI host loads. dbengine-sqlite and dbengine-clickhouse are v2-only; every other
# crate keeps ABI 1 exports (for the Swift app) unless built with `abi-v2`.
#
# resolve_driver <id|alias|crate>  sets: DRIVER_ID CRATE CARGO_FEATURE_ARGS DYLIB_NAME
# library_file_name <crate> [triple|target-id]   cdylib name on that target (default: this host)
# host_target                               manifest target id of this host, e.g. macos-aarch64
# target_os <target-id>                     macos | linux | windows
#
# Bash 3.2 compatible (macOS /bin/bash): no associative arrays.

ALL_DRIVER_IDS="mongodb postgresql mysql mariadb redis sqlite clickhouse"

resolve_driver() {
    CARGO_FEATURE_ARGS=""
    case "$1" in
        mongodb|mongo|dbengine-mongo)
            DRIVER_ID=mongodb; CRATE=dbengine-mongo; CARGO_FEATURE_ARGS="--features abi-v2" ;;
        postgresql|postgres|dbengine-postgres)
            DRIVER_ID=postgresql; CRATE=dbengine-postgres; CARGO_FEATURE_ARGS="--features abi-v2" ;;
        mysql|dbengine-mysql)
            DRIVER_ID=mysql; CRATE=dbengine-mysql; CARGO_FEATURE_ARGS="--features abi-v2" ;;
        mariadb|dbengine-mariadb)
            DRIVER_ID=mariadb; CRATE=dbengine-mariadb; CARGO_FEATURE_ARGS="--features abi-v2" ;;
        redis|dbengine-redis)
            DRIVER_ID=redis; CRATE=dbengine-redis; CARGO_FEATURE_ARGS="--features abi-v2" ;;
        sqlite|dbengine-sqlite)
            DRIVER_ID=sqlite; CRATE=dbengine-sqlite; CARGO_FEATURE_ARGS="" ;;
        clickhouse|dbengine-clickhouse)
            DRIVER_ID=clickhouse; CRATE=dbengine-clickhouse; CARGO_FEATURE_ARGS="" ;;
        *)
            echo "Unknown driver '$1' (known: ${ALL_DRIVER_IDS})" >&2
            return 1 ;;
    esac
    DYLIB_NAME="$(library_file_name "${CRATE}")"
}

# cdylib file name cargo produces: lib<name>.dylib (macOS), lib<name>.so
# (Linux), <name>.dll (Windows). `triple` (a rust target triple or a manifest target id such as linux-x86_64) overrides the host.
library_file_name() {
    local name triple="${2:-}"
    name="$(printf '%s' "$1" | tr '-' '_')"
    if [[ -z "${triple}" ]]; then
        case "$(uname -s)" in
            Darwin) triple=apple-darwin ;;
            MINGW*|MSYS*|CYGWIN*) triple=windows ;;
            *) triple=linux ;;
        esac
    fi
    case "${triple}" in
        *apple-darwin*|macos-*) printf 'lib%s.dylib' "${name}" ;;
        *windows*) printf '%s.dll' "${name}" ;;
        *) printf 'lib%s.so' "${name}" ;;
    esac
}

# Manifest target id (see scripts/driver_manifest.py) of the machine running this.
host_target() {
    local os arch
    case "$(uname -s)" in
        Darwin) os=macos ;;
        MINGW*|MSYS*|CYGWIN*) os=windows ;;
        *) os=linux ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64) arch=x86_64 ;;
        *) arch="$(uname -m)" ;;
    esac
    printf '%s-%s' "${os}" "${arch}"
}

target_os() {
    printf '%s' "${1%%-*}"
}

# Crate version from the [package] table of crates/<crate>/Cargo.toml.
crate_version() {
    awk -F'"' '/^\[package\]/ {p=1; next} /^\[/ {p=0} p && /^version[[:space:]]*=/ {print $2; exit}' \
        "$1/crates/$2/Cargo.toml"
}

cargo_bin() {
    if [[ -n "${CARGO:-}" ]]; then printf '%s' "${CARGO}"
    elif [[ -x "${HOME}/.cargo/bin/cargo" ]]; then printf '%s' "${HOME}/.cargo/bin/cargo"
    else command -v cargo; fi
}

host_arch() {
    case "$(uname -m)" in
        arm64|aarch64) printf 'arm64' ;;
        *) uname -m ;;
    esac
}
