#!/usr/bin/env python3
import json
import re
import sys
from pathlib import Path


SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def parse_version(value: str) -> tuple[int, int, int]:
    match = SEMVER_RE.match(value)
    if not match:
        raise ValueError(f"invalid semver: {value}")
    return tuple(int(part) for part in match.groups())


def format_version(version: tuple[int, int, int]) -> str:
    return f"{version[0]}.{version[1]}.{version[2]}"


def bump_version(version: tuple[int, int, int], bump: str) -> tuple[int, int, int]:
    major, minor, patch = version
    if bump == "patch":
        return major, minor, patch + 1
    if bump == "minor":
        return major, minor + 1, 0
    if bump == "major":
        return major + 1, 0, 0
    raise ValueError(f"invalid bump: {bump}")


def main() -> int:
    if len(sys.argv) not in (4, 5):
        print(
            "Usage: next-driver-version.py <manifest> <driver-id> <patch|minor|major> [version-override]",
            file=sys.stderr,
        )
        return 2

    manifest_path = Path(sys.argv[1])
    driver_id = sys.argv[2]
    bump = sys.argv[3]
    override = sys.argv[4].strip() if len(sys.argv) == 5 else ""

    if bump not in {"patch", "minor", "major"}:
        print(f"invalid bump: {bump}", file=sys.stderr)
        return 2

    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {"drivers": []}
    drivers = manifest.get("drivers", [])
    current = next((driver.get("version") for driver in drivers if driver.get("id") == driver_id), None)

    if current is None:
        next_version = parse_version(override) if override else (1, 0, 0)
    elif override:
        next_version = parse_version(override)
        if next_version <= parse_version(current):
            print(f"version override {override} must be greater than current {current}", file=sys.stderr)
            return 1
    else:
        next_version = bump_version(parse_version(current), bump)

    print(format_version(next_version))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
