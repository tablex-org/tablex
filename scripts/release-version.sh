#!/usr/bin/env bash
# Release build jobs: the pushed tag must name the app version of apps/tablex-gpui/Cargo.toml, and
# the notes of that release must be written. Appends VERSION=<version> to $GITHUB_ENV.
# A manual run (workflow_dispatch, a dry run that publishes nothing) has no tag to compare: it takes the
# version of Cargo.toml and skips both checks.
# Usage: scripts/release-version.sh   (env: GITHUB_EVENT_NAME, GITHUB_REF_NAME, GITHUB_ENV)
set -euo pipefail

version="$(sed -n 's/^version = "\(.*\)"/\1/p' apps/tablex-gpui/Cargo.toml | head -n 1)"
# The update manifest needs a plain major.minor.patch: fail here, not when publishing.
if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "the app version ${version} is not major.minor.patch" >&2
    exit 1
fi
if [[ "${GITHUB_EVENT_NAME:-}" == "workflow_dispatch" ]]; then
    echo "VERSION=${version}" >> "${GITHUB_ENV}"
    echo "dry run: building version ${version} of Cargo.toml, no tag check"
    exit 0
fi
if [[ "${GITHUB_REF_NAME}" != "v${version}" ]]; then
    echo "tag ${GITHUB_REF_NAME} does not match the app version ${version}" >&2
    exit 1
fi
echo "VERSION=${version}" >> "${GITHUB_ENV}"
# What changed, written before tagging; it opens the release page.
if [[ ! -s "release-notes/${GITHUB_REF_NAME}.md" ]]; then
    echo "release-notes/${GITHUB_REF_NAME}.md is missing or empty" >&2
    exit 1
fi
