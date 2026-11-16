#!/usr/bin/env bash
# Signs release builds (TableX.app, tablex-mcp) with the TableX release certificate: a self-signed
# code-signing certificate, kept for good, whose SHA-1 is pinned in scripts/release-signing.sha1 and in
# the app's updater. A build signed with it has the same designated requirement in every release
# (`identifier … and certificate root = H"<sha1>"`), so what a user allowed in the Keychain for one
# version still holds for the next (an ad-hoc signature is tied to the build's own hash instead).
# It is not an Apple Developer ID: Gatekeeper still asks for the first launch.
#
# Usage (CI, macOS):
#   scripts/release-signing.sh setup            # needs TABLEX_CODESIGN_P12 (base64) and
#                                               # TABLEX_CODESIGN_P12_PASSWORD; writes TABLEX_SIGN_KEYCHAIN
#                                               # and TABLEX_SIGN_IDENTITY to $GITHUB_ENV when set
#   scripts/release-signing.sh sign PATH [IDENTIFIER]
#   scripts/release-signing.sh verify PATH IDENTIFIER
#   scripts/release-signing.sh cleanup
set -euo pipefail

[ "$(uname -s)" = "Darwin" ] || { echo "release-signing.sh only runs on macOS" >&2; exit 1; }
here="$(cd "$(dirname "$0")" && pwd)"
pinned="$(tr -d '[:space:]' < "$here/release-signing.sha1" | tr '[:upper:]' '[:lower:]')"
keychain="${TABLEX_SIGN_KEYCHAIN:-${RUNNER_TEMP:-/tmp}/tablex-release-signing.keychain-db}"

requirement() {
    printf 'identifier "%s" and certificate root = H"%s"' "$1" "$pinned"
}

case "${1:-}" in
setup)
    : "${TABLEX_CODESIGN_P12:?the release certificate (base64 .p12) is not set}"
    : "${TABLEX_CODESIGN_P12_PASSWORD:?the release certificate password is not set}"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    printf '%s' "$TABLEX_CODESIGN_P12" | base64 --decode > "$work/cert.p12"
    password="$(openssl rand -hex 24)"
    rm -f "$keychain"
    security create-keychain -p "$password" "$keychain"
    security set-keychain-settings -lut 3600 "$keychain"
    security unlock-keychain -p "$password" "$keychain"
    security import "$work/cert.p12" -k "$keychain" -P "$TABLEX_CODESIGN_P12_PASSWORD" -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" >/dev/null
    # The one identity in it must be the pinned certificate: a different key would change what every
    # user allowed, and the updater refuses it.
    identity="$(security find-identity -p codesigning "$keychain" | awk '/^ *1\)/ {print tolower($2)}')"
    if [ "$identity" != "$pinned" ]; then
        echo "the release certificate is $identity, not the pinned $pinned" >&2
        security delete-keychain "$keychain" || true
        exit 1
    fi
    if [ -n "${GITHUB_ENV:-}" ]; then
        echo "TABLEX_SIGN_KEYCHAIN=$keychain" >> "$GITHUB_ENV"
        echo "TABLEX_SIGN_IDENTITY=$identity" >> "$GITHUB_ENV"
    fi
    echo "release signing identity $identity ready"
    ;;
sign)
    path="${2:?usage: $0 sign PATH [IDENTIFIER]}"
    args=(--force --keychain "$keychain" --sign "$pinned")
    [ -d "$path" ] && args+=(--deep)
    [ -n "${3:-}" ] && args+=(--identifier "$3")
    codesign "${args[@]}" "$path"
    ;;
verify)
    path="${2:?usage: $0 verify PATH IDENTIFIER}"
    identifier="${3:?usage: $0 verify PATH IDENTIFIER}"
    args=(--verify --strict)
    [ -d "$path" ] && args+=(--deep)
    codesign "${args[@]}" -R="$(requirement "$identifier")" "$path"
    codesign -dr - "$path" 2>&1 | grep -q "certificate root = H\"$pinned\"" ||
        { echo "$path is not signed with the release certificate" >&2; exit 1; }
    echo "$path: signed with the release certificate"
    ;;
cleanup)
    security delete-keychain "$keychain" 2>/dev/null || true
    ;;
*)
    echo "usage: $0 setup|sign PATH [IDENTIFIER]|verify PATH IDENTIFIER|cleanup" >&2
    exit 2
    ;;
esac
