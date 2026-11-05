#!/usr/bin/env python3
"""Add or replace one entry (a driver or a tool) in a manifest, keeping the rest.

  update-driver-manifest.py MANIFEST ENTRY.json
  update-driver-manifest.py MANIFEST --tool-binary FILE --version V --url URL
                            [--target T] [--tool-id tablex-mcp] [--min-app-version V]
                            [--requires-driver ID:MINVERSION]...

The second form builds the tool entry (size and SHA-256 of the built binary) the way
`driver_manifest.py tool-entry` does and then replaces the tool of that id. The header is
refreshed (schemaVersion, a generatedAt newer than the manifest being replaced); sign the result
LAST (`driver_manifest.py sign`).
"""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import driver_manifest  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("manifest")
    parser.add_argument("entry", nargs="?", help="a manifest entry file (driver or tool)")
    parser.add_argument("--tool-binary", help="a built tool executable: its entry is built here")
    parser.add_argument("--version")
    parser.add_argument("--url")
    parser.add_argument("--target", default="")
    parser.add_argument("--tool-id", default="tablex-mcp")
    parser.add_argument("--min-app-version", default=driver_manifest.DEFAULT_MIN_APP_VERSION)
    parser.add_argument("--requires-driver", action="append", default=[], metavar="ID:MINVERSION")
    args = parser.parse_args(argv)
    if bool(args.entry) == bool(args.tool_binary):
        parser.error("give either an ENTRY file or --tool-binary")
    if args.tool_binary and not (args.version and args.url):
        parser.error("--tool-binary needs --version and --url")

    manifest_path = Path(args.manifest)
    try:
        manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {"drivers": []}
        if args.tool_binary:
            entry = driver_manifest.build_tool_entry(
                Path(args.tool_binary),
                version=args.version,
                url=args.url,
                target=args.target or None,
                tool_id=args.tool_id,
                min_app_version=args.min_app_version,
                requires=driver_manifest.parse_requirements(args.requires_driver),
            )
        else:
            entry = json.loads(Path(args.entry).read_text())
        entry_id = entry.get("id")
        if not entry_id:
            print("manifest entry missing id", file=sys.stderr)
            return 1
        # Schema v2 header: schemaVersion and a generatedAt strictly newer than the manifest
        # being replaced.
        manifest = driver_manifest.merge_entries([entry], manifest)
    except (driver_manifest.ManifestError, OSError, json.JSONDecodeError) as error:
        print(f"update-driver-manifest.py: {error}", file=sys.stderr)
        return 1

    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n")
    print(f"Updated {manifest_path} with {entry_id} {entry.get('version')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
