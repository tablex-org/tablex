#!/usr/bin/env python3
"""Build driver manifest entries from what the dylib itself reports.

The `inspect-driver` example (crates/engine-driver-host/examples) loads a dylib
through the real host loader and prints JSON with the driver's capabilities.
This tool turns that JSON plus the packaged zip into a manifest entry, so the
`id`, `displayName`, `uriSchemes`, `appAbiVersion` and `features` fields can
never drift from the binary. Only licence, source repo, Team ID, URL and the
artifact checksum come from outside the dylib.

Schema v2 = engine-driver-host `DriverManifest` / `DriverManifestEntry` (camelCase).
Manifest: schemaVersion (2), generatedAt (RFC 3339 UTC, strictly increasing per
release), drivers, tools. Entry: id, displayName, version, appAbiVersion,
uriSchemes, features, license, sourceRepo, teamId, artifacts[{target, arch,
url, sha256, sizeBytes}], and the v2 catalog fields category (sql | nosql |
analytics | key_value), description, minAppVersion, releaseNotesUrl,
publishedAt.

Targets: macos-aarch64 | macos-x86_64 | macos-universal | linux-x86_64 |
linux-aarch64 | windows-x86_64 | windows-aarch64. `target` is what the host
selects by; `arch` is kept for old readers (arm64 | x86_64 | universal on
macOS builds, the target itself elsewhere so an old macOS app never matches it).
`teamId` is the macOS Team ID: required only when an entry has a macOS artifact.
`merge` folds entries of the same driver id and version (one per target, built
on different runners) into one entry with every artifact.

Dev path (`dev-drivers.sh`): `file://` artifact URLs, teamId "" for ad-hoc
signed dylibs, no signature (the host's Developer policy accepts that).
Release path: https URLs only, a Team ID, and the manifest signed LAST over its
final bytes with `sign` (any later rewrite invalidates the `.sig`).

Tools (`tools` in the manifest): the standalone MCP server `tablex-mcp` (kind
"mcpServer"), the SQL formatter `tablex-sqruff` (kind "sqlFormatter", free-standing; its entry names
the Apache-2.0 licence and the sqruff repository in `license` / `sourceRepo`) and the MongoDB Shell plugin `tablex-mongo-shell` (kind
"mongoShell", an add-on of the MongoDB driver: its entry carries
`requires: [{"driver": "mongodb", "minVersion": V}]`, which the app checks
before it installs the plugin and before it runs a script). One entry per tool
and one artifact per target. The artifact is the bare executable itself;
`tool-entry` computes its size and SHA-256 from the built binary. `merge` tells tool entries from driver entries by their
`kind` and replaces a tool of the same id; entries of other kinds that are
already in a manifest are carried over untouched.

Subcommands:
  entry  --inspect FILE --version V --zip FILE --url URL [--target T | --arch A]
         --team-id T --license L [--source-repo R] [--require JSON_LIST]
         [--min-app-version V] [--release-notes-url URL] [--published-at TS]
         --out FILE
  tool-entry --binary FILE --version V --url URL [--target T] [--id tablex-mcp]
         [--display-name N] [--description D] [--min-app-version V]
         [--requires-driver ID:MINVERSION]... --out FILE   (an add-on must name its driver)
  merge  --out MANIFEST [--release] ENTRY.json...   (--release: refuse file://
         URLs and a missing Team ID on macOS artifacts; entries may be driver
         or tool entries)
  sign   --manifest FILE (--key FILE | --key-env VAR)   (release only; writes
         FILE.sig with the engine-driver-host `sign-manifest` example)
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

EXPECTED_ABI = 2
SCHEMA_VERSION = 2
DEFAULT_SOURCE_REPO = "https://github.com/tablex-org/tablex"
DEFAULT_MIN_APP_VERSION = "0.1.0"
CATEGORIES = ("sql", "nosql", "analytics", "key_value")
# Tools the host knows: id -> (kind, display name, description, drivers it must require). The host
# ignores any other id, and a kind that does not belong to the id.
TOOL_META = {
    "tablex-mcp": (
        "mcpServer",
        "TableX MCP server",
        "Lets MCP clients (Claude Code, Claude Desktop, Cursor) use your saved connections, within each connection's assistant access.",
        (),
    ),
    "tablex-mongo-shell": (
        "mongoShell",
        "MongoDB Shell",
        "Runs mongosh-style scripts (loops, inserts, helpers, show and use) in a MongoDB query tab, in a separate process.",
        ("mongodb",),
    ),
    "tablex-sqruff": (
        "sqlFormatter",
        "SQL Formatter (sqruff)",
        "Formats SQL in the editor with sqruff, in a separate process. Layout and keyword case only; the built-in formatter stays the default.",
        (),
    ),
}
# Licence and source of what a tool is made of, shown before it is installed: id -> (SPDX licence,
# source repository). A tool built on third-party code names that code, not only TableX.
TOOL_SOURCES = {
    "tablex-mcp": ("AGPL-3.0-only", DEFAULT_SOURCE_REPO),
    "tablex-mongo-shell": ("AGPL-3.0-only", DEFAULT_SOURCE_REPO),
    "tablex-sqruff": ("Apache-2.0", "https://github.com/quarylabs/sqruff"),
}
TOOL_KINDS = {meta[0] for meta in TOOL_META.values()}
MAX_TOOL_BYTES = 256 * 1024 * 1024  # the host's MAX_TOOL_BYTES
ROOT = Path(__file__).resolve().parent.parent
TARGET_PATTERN = re.compile(r"(macos-(aarch64|x86_64|universal)|linux-(aarch64|x86_64)|windows-(aarch64|x86_64))")
# Legacy `arch` values (always macOS) <-> the macOS targets.
LEGACY_ARCH_TO_TARGET = {"arm64": "macos-aarch64", "x86_64": "macos-x86_64", "universal": "macos-universal"}

# Catalog metadata the dylib does not carry: driver id -> (category, description).
DRIVER_META = {
    "postgresql": ("sql", "PostgreSQL relational database over the native protocol."),
    "mysql": ("sql", "MySQL relational database over the native protocol."),
    "mariadb": ("sql", "MariaDB relational database over the native protocol."),
    "sqlite": ("sql", "SQLite embedded database files, read and written in place."),
    "mongodb": ("nosql", "MongoDB document database with find, aggregate and script queries."),
    "redis": ("key_value", "Redis key-value store with typed key browsing and commands."),
    "clickhouse": ("analytics", "ClickHouse column-store analytics database over the HTTP interface."),
}


class ManifestError(Exception):
    pass


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def parse_timestamp(text: str) -> datetime.datetime:
    """RFC 3339 UTC with a `Z` suffix and whole seconds, the only form the tooling writes."""
    try:
        return datetime.datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ")
    except (TypeError, ValueError) as error:
        raise ManifestError(f"{text!r} is not an RFC 3339 UTC timestamp (YYYY-MM-DDTHH:MM:SSZ)") from error


def host_target() -> str:
    """Target of the machine running this script (the build host)."""
    system = {"darwin": "macos", "linux": "linux", "windows": "windows"}.get(platform.system().lower())
    machine = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}.get(platform.machine().lower())
    if not system or not machine:
        raise ManifestError(f"cannot derive a target from {platform.system()}/{platform.machine()}; pass --target")
    return f"{system}-{machine}"


def resolve_target(target: str | None = None, arch: str | None = None) -> str:
    """`target` wins; else a legacy macOS `arch`; else the build host."""
    if not target:
        if arch:
            target = LEGACY_ARCH_TO_TARGET.get(arch)
            if not target:
                raise ManifestError(f"unknown --arch {arch!r}; use --target (one of {sorted(LEGACY_ARCH_TO_TARGET.values())})")
        else:
            target = host_target()
    if not TARGET_PATTERN.fullmatch(target):
        raise ManifestError(f"unknown target {target!r} (macos-aarch64, macos-x86_64, macos-universal, linux-x86_64, linux-aarch64, windows-x86_64, windows-aarch64)")
    return target


def legacy_arch(target: str) -> str:
    """`arch` for readers that predate `target`: the macOS value, or the target
    itself for other OSes (an old macOS app never matches that)."""
    for arch, mapped in LEGACY_ARCH_TO_TARGET.items():
        if mapped == target:
            return arch
    return target


def ad_hoc_signing() -> bool:
    """`TABLEX_AD_HOC_SIGNING=1`: a release made without a Developer ID. Its macOS
    builds carry an ad-hoc signature and an empty Team ID, which the host accepts
    from the signed manifest (whose SHA-256 pins each file). Off, a macOS build
    without a Team ID is refused, so a missing certificate is never silent."""
    return os.environ.get("TABLEX_AD_HOC_SIGNING") == "1"


def check_url_and_team(url: str, team_id: str, target: str = "macos-aarch64") -> None:
    """`file://` is Developer-only; anything else must be https, and Team-ID
    signed when it is a macOS build (Linux and Windows have no Team ID), unless
    the release is ad hoc signed ([`ad_hoc_signing`])."""
    if url.startswith("file://"):
        return
    if not url.startswith("https://"):
        raise ManifestError(f"artifact URL must be https:// (or file:// for development): {url}")
    if target.startswith("macos-") and not team_id and not ad_hoc_signing():
        raise ManifestError(f"an https artifact needs a Team ID: {url}")


def build_entry(
    inspect: dict,
    *,
    version: str,
    zip_path: Path,
    url: str,
    arch: str | None = None,
    target: str | None = None,
    team_id: str = "",
    license: str = "",
    source_repo: str = DEFAULT_SOURCE_REPO,
    require: list[str] | None = None,
    category: str | None = None,
    description: str | None = None,
    min_app_version: str = DEFAULT_MIN_APP_VERSION,
    release_notes_url: str | None = None,
    published_at: str | None = None,
) -> dict:
    """Entry for one driver. `inspect` is the JSON printed by `inspect-driver <dylib>`."""
    caps = inspect.get("capabilities")
    if not isinstance(caps, dict):
        raise ManifestError("inspect output has no 'capabilities' object")
    abi = caps.get("abi_version")
    if abi != EXPECTED_ABI:
        raise ManifestError(f"driver reports ABI {abi!r}, host requires {EXPECTED_ABI}")
    driver = caps.get("driver")
    if not driver or not isinstance(driver, str):
        raise ManifestError("capabilities.driver is missing")
    features = caps.get("features")
    if not isinstance(features, list) or not all(isinstance(f, str) for f in features):
        raise ManifestError("capabilities.features must be a list of strings")
    if len(set(features)) != len(features):
        raise ManifestError("capabilities.features contains duplicates")
    schemes = caps.get("uri_schemes")
    if not isinstance(schemes, list) or not schemes:
        raise ManifestError("capabilities.uri_schemes must be a non-empty list")
    missing = [token for token in (require or []) if token not in features]
    if missing:
        raise ManifestError(f"{driver}: expected features missing from dylib: {missing}")
    target = resolve_target(target, arch)
    for field, value in (("version", version), ("url", url), ("license", license)):
        if not value:
            raise ManifestError(f"{field} must not be empty")
    check_url_and_team(url, team_id, target)
    default_category, default_description = DRIVER_META.get(driver, (None, None))
    category = category or default_category
    if category not in CATEGORIES:
        raise ManifestError(f"{driver}: category must be one of {CATEGORIES}, got {category!r}")
    description = description or default_description or caps.get("display_name") or driver
    if "\n" in description:
        raise ManifestError(f"{driver}: description must be one line")
    if not re.fullmatch(r"\d+(\.\d+)*", min_app_version):
        raise ManifestError(f"minAppVersion must be dotted numeric, got {min_app_version!r}")
    published_at = published_at or utc_now()
    parse_timestamp(published_at)
    return {
        "id": driver,
        "displayName": caps.get("display_name") or driver,
        "version": version,
        "appAbiVersion": abi,
        "uriSchemes": list(schemes),
        "features": list(features),
        "license": license,
        "sourceRepo": source_repo,
        "teamId": team_id,
        "artifacts": [
            {
                "target": target,
                "arch": legacy_arch(target),
                "url": url,
                "sha256": sha256_file(zip_path),
                "sizeBytes": zip_path.stat().st_size,
            }
        ],
        "category": category,
        "description": description,
        "minAppVersion": min_app_version,
        "releaseNotesUrl": release_notes_url or f"{source_repo}/releases/tag/{driver}-v{version}",
        "publishedAt": published_at,
    }


def build_tool_entry(
    binary: Path,
    *,
    version: str,
    url: str,
    target: str | None = None,
    tool_id: str = "tablex-mcp",
    display_name: str | None = None,
    description: str | None = None,
    min_app_version: str = DEFAULT_MIN_APP_VERSION,
    requires: list[tuple[str, str]] | None = None,
) -> dict:
    """Entry for one build of a tool; size and SHA-256 are computed from the built `binary`
    (the file users download, so sign / strip it BEFORE calling this). `requires` is
    `[(driver id, minimum version)]`: what an add-on needs installed."""
    if tool_id not in TOOL_META:
        raise ManifestError(f"unknown tool {tool_id!r} (known: {sorted(TOOL_META)})")
    requires = requires or []
    kind, default_name, default_description, must_require = TOOL_META[tool_id]
    license, source_repo = TOOL_SOURCES[tool_id]
    for driver in must_require:
        if not any(name == driver for name, _ in requires):
            raise ManifestError(f"{tool_id} is an add-on of the {driver} driver: pass --requires-driver {driver}:MINVERSION")
    for name, minimum in requires:
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", name):
            raise ManifestError(f"required driver id {name!r} is not a driver id")
        if not re.fullmatch(r"\d+(\.\d+)*", minimum):
            raise ManifestError(f"minimum version of {name} must be dotted numeric, got {minimum!r}")
    if not binary.is_file():
        raise ManifestError(f"{binary} is not a file")
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,32}", version):
        raise ManifestError(f"version must be 1-32 characters of letters, digits . _ -, got {version!r}")
    if not re.fullmatch(r"\d+(\.\d+)*", min_app_version):
        raise ManifestError(f"minAppVersion must be dotted numeric, got {min_app_version!r}")
    target = resolve_target(target)
    if not url:
        raise ManifestError("url must not be empty")
    if not url.startswith(("https://", "file://")):
        raise ManifestError(f"artifact URL must be https:// (or file:// for development): {url}")
    size = binary.stat().st_size
    if not 0 < size <= MAX_TOOL_BYTES:
        raise ManifestError(f"{binary} is {size} bytes; a tool must be 1..{MAX_TOOL_BYTES}")
    entry = {
        "id": tool_id,
        "kind": kind,
        "displayName": display_name or default_name,
        "version": version,
        "description": description or default_description,
        "minAppVersion": min_app_version,
        "license": license,
        "sourceRepo": source_repo,
        "requires": [{"driver": name, "minVersion": minimum} for name, minimum in requires],
        "artifacts": [
            {
                "target": target,
                "arch": legacy_arch(target),
                "url": url,
                "sha256": sha256_file(binary),
                "sizeBytes": size,
            }
        ],
    }
    if not requires:
        del entry["requires"]  # a free-standing tool has no such field
    return entry


def is_tool_entry(entry: dict) -> bool:
    return isinstance(entry, dict) and entry.get("kind") in TOOL_KINDS


def combine_targets(entries: list[dict]) -> list[dict]:
    """One entry per driver id: per-target entries of the same id and version
    (one per runner) are folded into the first, keeping every artifact. A
    non-empty Team ID wins over an empty one; two builds of one target or two
    versions of one id are an error."""
    combined: dict[str, dict] = {}
    for entry in entries:
        current = combined.get(entry["id"])
        if current is None:
            combined[entry["id"]] = dict(entry, artifacts=list(entry.get("artifacts", [])))
            continue
        if current.get("version") != entry.get("version"):
            raise ManifestError(f"{entry['id']}: entries disagree on version ({current.get('version')!r} vs {entry.get('version')!r})")
        known = {artifact_target(a) for a in current["artifacts"]}
        for artifact in entry.get("artifacts", []):
            if artifact_target(artifact) in known:
                raise ManifestError(f"{entry['id']}: two artifacts for target {artifact_target(artifact)}")
            current["artifacts"].append(artifact)
            known.add(artifact_target(artifact))
        if not current.get("teamId") and entry.get("teamId"):
            current["teamId"] = entry["teamId"]
    return list(combined.values())


def artifact_target(artifact: dict) -> str:
    """Target of a manifest artifact; `arch`-only (old) artifacts are macOS."""
    return artifact.get("target") or LEGACY_ARCH_TO_TARGET.get(artifact.get("arch", ""), "macos-" + artifact.get("arch", ""))


def merge_entries(entries: list[dict], existing: dict | None = None, now: str | None = None) -> dict:
    """Manifest with `entries` replacing same-id drivers of `existing`, sorted by id.

    `generatedAt` is always present and strictly greater than the one of
    `existing` (a manifest must never go backwards or repeat): when the clock
    has not moved past it, it is `existing`'s plus one second.
    """
    manifest = dict(existing or {})
    by_id = {d["id"]: d for d in manifest.get("drivers", [])}
    new_tools = []
    for entry in combine_targets(entries):
        if is_tool_entry(entry):
            new_tools.append(entry)
        else:
            by_id[entry["id"]] = entry
    stamp = parse_timestamp(now) if now else parse_timestamp(utc_now())
    if manifest.get("generatedAt"):
        floor = parse_timestamp(manifest["generatedAt"])
        if stamp <= floor:
            stamp = floor + datetime.timedelta(seconds=1)
    manifest["schemaVersion"] = SCHEMA_VERSION
    manifest["generatedAt"] = stamp.strftime("%Y-%m-%dT%H:%M:%SZ")
    manifest["drivers"] = [by_id[key] for key in sorted(by_id)]
    # A tool of the same id is replaced; every other entry (including kinds this script does
    # not know) is carried over untouched.
    replaced = {tool["id"] for tool in new_tools}
    kept = [tool for tool in manifest.get("tools", []) if not (is_tool_entry(tool) and tool.get("id") in replaced)]
    manifest["tools"] = kept + sorted(new_tools, key=lambda tool: tool["id"])
    return manifest


def validate_release(manifest: dict) -> None:
    """What a signed production manifest must satisfy (the host re-checks all of it)."""
    if manifest.get("schemaVersion") != SCHEMA_VERSION:
        raise ManifestError(f"schemaVersion must be {SCHEMA_VERSION}")
    parse_timestamp(manifest.get("generatedAt"))
    if not manifest.get("drivers"):
        raise ManifestError("the manifest lists no drivers")
    for driver in manifest["drivers"]:
        for artifact in driver.get("artifacts", []):
            if not str(artifact.get("url", "")).startswith("https://"):
                raise ManifestError(f"{driver.get('id')}: release artifact URLs must be https: {artifact.get('url')}")
        macos = any(artifact_target(a).startswith("macos-") for a in driver.get("artifacts", []))
        if macos and not driver.get("teamId") and not ad_hoc_signing():
            raise ManifestError(f"{driver.get('id')}: a release with a macOS build needs a Team ID")
        for field in ("category", "description", "minAppVersion", "releaseNotesUrl", "publishedAt"):
            if not driver.get(field):
                raise ManifestError(f"{driver.get('id')}: {field} is missing")
    for tool in manifest.get("tools", []):
        if is_tool_entry(tool):
            validate_tool_release(tool)


def validate_tool_release(tool: dict) -> None:
    """What the host requires of a tool entry, plus https URLs for a release."""
    name = tool.get("id")
    if name not in TOOL_META:
        raise ManifestError(f"tool {name!r} is not one this build knows ({sorted(TOOL_META)}); the host would ignore it")
    kind, _, _, must_require = TOOL_META[name]
    if tool.get("kind") != kind:
        raise ManifestError(f"{name}: kind must be {kind!r}, got {tool.get('kind')!r}; the host would ignore it")
    for field in ("displayName", "version", "minAppVersion"):
        if not tool.get(field):
            raise ManifestError(f"{name}: {field} is missing")
    requires = tool.get("requires") or []
    for driver in must_require:
        if not any(isinstance(item, dict) and item.get("driver") == driver for item in requires):
            raise ManifestError(f"{name}: must require the {driver} driver")
    for item in requires:
        if not (isinstance(item, dict) and re.fullmatch(r"\d+(\.\d+)*", str(item.get("minVersion", "")))):
            raise ManifestError(f"{name}: a requirement needs a dotted numeric minVersion: {item!r}")
    artifacts = tool.get("artifacts") or []
    if not artifacts:
        raise ManifestError(f"{name}: no artifacts")
    seen = set()
    for artifact in artifacts:
        target = artifact_target(artifact)
        if not str(artifact.get("url", "")).startswith("https://"):
            raise ManifestError(f"{name}: release artifact URLs must be https: {artifact.get('url')}")
        if not re.fullmatch(r"[0-9a-fA-F]{64}", str(artifact.get("sha256", ""))):
            raise ManifestError(f"{name}: {target} has no valid sha256")
        size = artifact.get("sizeBytes")
        if not isinstance(size, int) or isinstance(size, bool) or not 0 < size <= MAX_TOOL_BYTES:
            raise ManifestError(f"{name}: {target} has an invalid sizeBytes {size!r}")
        if target in seen:
            raise ManifestError(f"{name}: two artifacts for target {target}")
        seen.add(target)


def sign_manifest(manifest_path: Path, key_path: Path | None, cargo: str = "cargo", key_text: str | None = None) -> Path:
    """Writes `<manifest>.sig` with the host's `sign-manifest` example (ed25519 over
    the exact bytes of the file, so call this after the manifest is final). The key is a file
    (`key_path`) or, with `key_text`, passed on the example's standard input so that a secret
    never touches the disk. `--locked`: the signing build uses exactly the committed lock file."""
    command = [
        cargo, "run", "--locked", "-q", "--release", "--manifest-path", str(ROOT / "rust-engine" / "Cargo.toml"),
        "-p", "engine-driver-host", "--example", "sign-manifest", "--",
        "sign", "-" if key_text is not None else str(key_path), str(manifest_path),
    ]
    result = subprocess.run(command, capture_output=True, text=True, input=key_text)
    if result.returncode != 0:
        raise ManifestError(f"sign-manifest failed: {result.stderr.strip() or result.stdout.strip()}")
    sig = Path(str(manifest_path) + ".sig")
    if not sig.exists():
        raise ManifestError(f"sign-manifest did not write {sig}")
    return sig


def _cmd_entry(args: argparse.Namespace) -> int:
    inspect = json.loads(Path(args.inspect).read_text())
    require = json.loads(args.require) if args.require else []
    entry = build_entry(
        inspect,
        version=args.version,
        zip_path=Path(args.zip),
        url=args.url,
        arch=args.arch or None,
        target=args.target or None,
        team_id=args.team_id,
        license=args.license,
        source_repo=args.source_repo,
        require=require,
        min_app_version=args.min_app_version,
        release_notes_url=args.release_notes_url or None,
        published_at=args.published_at or None,
    )
    Path(args.out).write_text(json.dumps(entry, indent=2) + "\n")
    print(f"Wrote {args.out}: {entry['id']} {entry['version']} ({len(entry['features'])} features)")
    return 0


def parse_requirements(items: list[str] | None) -> list[tuple[str, str]]:
    """`["mongodb:0.2.0"]` -> `[("mongodb", "0.2.0")]`."""
    parsed = []
    for item in items or []:
        name, separator, minimum = item.partition(":")
        if not separator or not name or not minimum:
            raise ManifestError(f"--requires-driver wants ID:MINVERSION, got {item!r}")
        parsed.append((name, minimum))
    return parsed


def _cmd_tool_entry(args: argparse.Namespace) -> int:
    entry = build_tool_entry(
        Path(args.binary),
        version=args.version,
        url=args.url,
        target=args.target or None,
        tool_id=args.id,
        display_name=args.display_name or None,
        description=args.description or None,
        min_app_version=args.min_app_version,
        requires=parse_requirements(args.requires_driver),
    )
    Path(args.out).write_text(json.dumps(entry, indent=2) + "\n")
    artifact = entry["artifacts"][0]
    print(f"Wrote {args.out}: {entry['id']} {entry['version']} {artifact['target']} ({artifact['sizeBytes']} bytes)")
    return 0


def _cmd_merge(args: argparse.Namespace) -> int:
    if not args.entries:
        raise ManifestError("no entry files given")
    entries = [json.loads(Path(p).read_text()) for p in args.entries]
    out = Path(args.out)
    # Rebuilt from scratch (stale drivers never linger), but never older than the
    # manifest it replaces.
    previous = json.loads(out.read_text()).get("generatedAt") if out.exists() else None
    manifest = merge_entries(entries, {"generatedAt": previous} if previous else None)
    if args.release:
        validate_release(manifest)
    out.write_text(json.dumps(manifest, indent=2) + "\n")
    listed = [d["id"] for d in manifest["drivers"]] + [t["id"] for t in manifest["tools"] if isinstance(t, dict) and "id" in t]
    print(f"Wrote {args.out}: {', '.join(listed)} (generatedAt {manifest['generatedAt']})")
    return 0


def _cmd_sign(args: argparse.Namespace) -> int:
    manifest_path = Path(args.manifest)
    validate_release(json.loads(manifest_path.read_text()))
    if args.key:
        sig = sign_manifest(manifest_path, Path(args.key), args.cargo)
    else:
        secret = os.environ.get(args.key_env or "")
        if not secret:
            raise ManifestError(f"environment variable {args.key_env} is empty or unset")
        # Standard input of the signing process: never written to disk, never on a command line.
        sig = sign_manifest(manifest_path, None, args.cargo, key_text=secret.strip() + "\n")
    print(f"Signed {manifest_path}: {sig}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    entry = sub.add_parser("entry")
    entry.add_argument("--inspect", required=True)
    entry.add_argument("--version", required=True)
    entry.add_argument("--zip", required=True)
    entry.add_argument("--url", required=True)
    entry.add_argument("--target", default="", help="macos-aarch64 | macos-x86_64 | macos-universal | linux-x86_64 | linux-aarch64 | windows-x86_64 | windows-aarch64 (default: the build host)")
    entry.add_argument("--arch", default="", help="legacy macOS arch (arm64 | x86_64 | universal); --target wins")
    entry.add_argument("--team-id", default="", help="macOS Team ID (required for macOS https builds)")
    entry.add_argument("--license", required=True)
    entry.add_argument("--source-repo", default=DEFAULT_SOURCE_REPO)
    entry.add_argument("--require", default="", help="JSON list of feature tokens the dylib must advertise")
    entry.add_argument("--min-app-version", default=DEFAULT_MIN_APP_VERSION)
    entry.add_argument("--release-notes-url", default="")
    entry.add_argument("--published-at", default="")
    entry.add_argument("--out", required=True)
    entry.set_defaults(func=_cmd_entry)

    tool = sub.add_parser("tool-entry", help="manifest entry for one build of a tool (computes size and SHA-256)")
    tool.add_argument("--binary", required=True, help="the built, signed executable that users will download")
    tool.add_argument("--version", required=True)
    tool.add_argument("--url", required=True)
    tool.add_argument("--target", default="", help="same targets as `entry` (default: the build host)")
    tool.add_argument("--id", default="tablex-mcp")
    tool.add_argument("--display-name", default="")
    tool.add_argument("--description", default="")
    tool.add_argument("--min-app-version", default=DEFAULT_MIN_APP_VERSION)
    tool.add_argument("--requires-driver", action="append", default=[], metavar="ID:MINVERSION", help="a driver the tool is an add-on of (repeatable)")
    tool.add_argument("--out", required=True)
    tool.set_defaults(func=_cmd_tool_entry)

    merge = sub.add_parser("merge")
    merge.add_argument("--out", required=True)
    merge.add_argument("--release", action="store_true", help="require https URLs and Team IDs")
    merge.add_argument("entries", nargs="*")
    merge.set_defaults(func=_cmd_merge)

    sign = sub.add_parser("sign", help="sign the final release manifest (writes <manifest>.sig)")
    sign.add_argument("--manifest", required=True)
    group = sign.add_mutually_exclusive_group(required=True)
    group.add_argument("--key", help="key file (never commit one)")
    group.add_argument("--key-env", help="environment variable holding the base64 key (a CI secret)")
    sign.add_argument("--cargo", default=os.environ.get("CARGO", "cargo"))
    sign.set_defaults(func=_cmd_sign)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (ManifestError, OSError, json.JSONDecodeError) as error:
        print(f"driver_manifest.py: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
