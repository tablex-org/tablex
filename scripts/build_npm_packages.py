#!/usr/bin/env python3
"""Assembles the npm packages of tablex-mcp from the binaries a drivers release published.

    python3 scripts/build_npm_packages.py --binaries DIR --out DIR [--manifest FILE] [--pack]
                                          [--version V] [--require TARGET]...

DIR holds `tablex-mcp-<target>[.exe]`, the files `release-drivers.yml` uploads as release assets
(macOS builds already signed). Each becomes the one file of a platform package
(`@tablex/mcp-darwin-arm64`, ...), byte for byte: the SHA-256 of the file in the package is the
SHA-256 of the release asset, and with `--manifest` (the release's signed `drivers-manifest.json`)
the script refuses to go on when an asset is not the one the manifest names. The launcher package
`@tablex/mcp-server` (from packages/mcp-server) gets those SHA-256 values in `checksums.json`, which
its launcher checks before it runs a binary.

Nothing here publishes: `--pack` runs `npm pack` (a tarball per package, platform packages first)
and stops there.
"""

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "packages" / "mcp-server"
CARGO_TOML = ROOT / "apps" / "tablex-mcp" / "Cargo.toml"
TOOL_ID = "tablex-mcp"
HOMEPAGE = "https://github.com/tablex-org/tablex/tree/main/apps/tablex-mcp"
REPOSITORY = "git+https://github.com/tablex-org/tablex.git"


class BuildError(Exception):
    pass


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def crate_version(cargo_toml: Path = CARGO_TOML) -> str:
    return tomllib.loads(cargo_toml.read_text())["package"]["version"]


def load_platforms(source: Path = SOURCE) -> list[dict]:
    return json.loads((source / "platforms.json").read_text())["platforms"]


def asset_name(platform: dict) -> str:
    return f"{TOOL_ID}-{platform['target']}" + (".exe" if platform["binary"].endswith(".exe") else "")


def check_against_manifest(manifest_path: Path, version: str, built: dict[str, str]) -> None:
    """`built` maps target to the SHA-256 of the file about to be packed. Every one must be what
    the manifest lists for the tool at this version."""
    manifest = json.loads(manifest_path.read_text())
    tool = next((t for t in manifest.get("tools", []) if t.get("id") == TOOL_ID), None)
    if tool is None:
        raise BuildError(f"{manifest_path} lists no {TOOL_ID} tool")
    if tool.get("version") != version:
        raise BuildError(f"the manifest has {TOOL_ID} {tool.get('version')}, this is {version}")
    listed = {a.get("target"): str(a.get("sha256", "")).lower() for a in tool.get("artifacts", [])}
    for target, sha in sorted(built.items()):
        if target not in listed:
            raise BuildError(f"the manifest has no {TOOL_ID} artifact for {target}")
        if listed[target] != sha:
            raise BuildError(f"{TOOL_ID}-{target}: SHA-256 {sha} is not the manifest's {listed[target]}")


def write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def platform_package_json(platform: dict, version: str, sha: str) -> dict:
    package = {
        "name": platform["package"],
        "version": version,
        "description": f"The tablex-mcp binary for {platform['target']} (installed by @tablex/mcp-server; do not depend on it directly)",
        "license": "AGPL-3.0-only",
        "homepage": HOMEPAGE,
        "repository": {"type": "git", "url": REPOSITORY},
        "os": [platform["os"]],
        "cpu": [platform["cpu"]],
        "files": ["bin", "LICENSE"],
        "tablex": {"target": platform["target"], "sha256": sha},
    }
    if "libc" in platform:
        package["libc"] = [platform["libc"]]
    return package


def build(
    binaries: Path,
    out: Path,
    version: str,
    manifest: Path | None = None,
    require: tuple[str, ...] = (),
    source: Path = SOURCE,
    license_file: Path = ROOT / "LICENSE",
) -> list[Path]:
    """Writes the package folders under `out`; returns them, platform packages first, the launcher last."""
    platforms = load_platforms(source)
    known = {p["target"] for p in platforms}
    for target in require:
        if target not in known:
            raise BuildError(f"--require {target}: not a target of platforms.json ({sorted(known)})")
    found = {}
    for platform in platforms:
        path = binaries / asset_name(platform)
        if path.is_file():
            found[platform["target"]] = (platform, path)
        elif platform["target"] in require:
            raise BuildError(f"{path} is missing and {platform['target']} is required")
        else:
            print(f"no {asset_name(platform)} in {binaries}: no package for {platform['target']}")
    if not found:
        raise BuildError(f"no {TOOL_ID}-<target> binary in {binaries}")
    shas = {target: sha256_file(path) for target, (_, path) in found.items()}
    if manifest is not None:
        check_against_manifest(manifest, version, shas)

    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    folders = []
    checksums = {}
    for target, (platform, path) in found.items():
        folder = out / platform["package"].split("/")[-1]
        (folder / "bin").mkdir(parents=True)
        binary = folder / "bin" / platform["binary"]
        shutil.copyfile(path, binary)
        binary.chmod(0o755)
        if sha256_file(binary) != shas[target]:
            raise BuildError(f"{binary} differs from {path} after the copy")
        shutil.copyfile(license_file, folder / "LICENSE")
        write_json(folder / "package.json", platform_package_json(platform, version, shas[target]))
        checksums[platform["package"]] = shas[target]
        folders.append(folder)

    launcher = out / "mcp-server"
    for name in ("bin", "lib"):
        shutil.copytree(source / name, launcher / name)
    for name in ("platforms.json", "README.md"):
        if (source / name).is_file():
            shutil.copyfile(source / name, launcher / name)
    shutil.copyfile(license_file, launcher / "LICENSE")
    (launcher / "bin" / "tablex-mcp.js").chmod(0o755)
    package = json.loads((source / "package.json").read_text())
    package["version"] = version
    # Only the platforms that were built are dependencies: one that is not published must not make
    # an install fail. The launcher still knows all of them, and says so for a machine without one.
    package["optionalDependencies"] = {
        platform["package"]: version for platform, _ in found.values()
    }
    write_json(launcher / "package.json", package)
    write_json(launcher / "checksums.json", dict(sorted(checksums.items())))
    registry = json.loads((source / "server.json").read_text())
    registry["version"] = version
    for entry in registry["packages"]:
        entry["version"] = version
    write_json(launcher / "server.json", registry)
    return folders + [launcher]


def pack(folders: list[Path], destination: Path) -> list[Path]:
    npm = shutil.which("npm")
    if npm is None:
        raise BuildError("--pack needs npm on the PATH")
    destination.mkdir(parents=True, exist_ok=True)
    tarballs = []
    for folder in folders:
        result = subprocess.run(
            [npm, "pack", "--silent", "--pack-destination", str(destination.resolve())],
            cwd=folder, capture_output=True, text=True, check=False,
        )
        if result.returncode != 0:
            raise BuildError(f"npm pack failed in {folder}:\n{result.stderr}")
        tarballs.append(destination / result.stdout.strip().splitlines()[-1])
    return tarballs


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binaries", type=Path, required=True, help="folder with tablex-mcp-<target>[.exe]")
    parser.add_argument("--out", type=Path, required=True, help="folder to write the packages to (replaced)")
    parser.add_argument("--version", help="package version (default: the version in apps/tablex-mcp/Cargo.toml)")
    parser.add_argument("--manifest", type=Path, help="the release's drivers-manifest.json to check every binary against")
    parser.add_argument("--require", action="append", default=[], metavar="TARGET", help="fail when this target has no binary")
    parser.add_argument("--pack", action="store_true", help="run `npm pack` on each package into OUT/tarballs")
    args = parser.parse_args(argv)
    try:
        version = args.version or crate_version()
        folders = build(args.binaries, args.out, version, args.manifest, tuple(args.require))
        for folder in folders:
            print(f"wrote {folder}")
        if args.pack:
            for tarball in pack(folders, args.out / "tarballs"):
                print(f"packed {tarball}")
    except (BuildError, OSError, KeyError, json.JSONDecodeError) as error:
        print(f"build_npm_packages: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
