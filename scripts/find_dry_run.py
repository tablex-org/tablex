#!/usr/bin/env python3
"""Find the dry run a release (a tag push) can reuse instead of building again.

A dry run is a successful `workflow_dispatch` run of the same workflow. It is reusable when it ran on
the commit the tag points to (`headSha`) and every artifact the release needs is still there (an
artifact of a dry run expires after 3 days). The newest such run wins.

  python3 scripts/find_dry_run.py --repo OWNER/NAME --workflow release-app.yml --sha <commit> \\
      --require app-apple-silicon app-intel ...

Writes `run_id=<id>` to $GITHUB_OUTPUT (and to standard output); the id is empty when there is nothing
to reuse. Any failure of `gh` is a warning and an empty id: the release then builds, as it always did.
Needs `gh` and GH_TOKEN with `actions: read`.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from typing import Callable, Iterable


def find_run(runs: Iterable[dict], artifacts_of: Callable[[int], set[str]], sha: str, required: Iterable[str]) -> int | None:
    """The newest run of `runs` built from `sha` that still has every artifact named in `required`."""
    required = list(required)
    for run in sorted(runs, key=lambda run: run["createdAt"], reverse=True):
        if run["headSha"] != sha:
            continue
        missing = [name for name in required if name not in artifacts_of(run["databaseId"])]
        if not missing:
            return run["databaseId"]
        print(f"dry run {run['databaseId']} is for this commit but lacks {', '.join(missing)}", file=sys.stderr)
    return None


def gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", required=True, help="OWNER/NAME")
    parser.add_argument("--workflow", required=True, help="workflow file name, e.g. release-app.yml")
    parser.add_argument("--sha", required=True, help="the commit of the tag")
    parser.add_argument("--require", nargs="+", required=True, metavar="ARTIFACT", help="artifacts the release needs")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        print(f"--sha {args.sha!r} is not a full commit hash", file=sys.stderr)
        return 2

    def artifacts_of(run_id: int) -> set[str]:
        out = gh(
            "api", "--paginate", f"repos/{args.repo}/actions/runs/{run_id}/artifacts?per_page=100",
            "--jq", ".artifacts[] | select(.expired | not) | .name",
        )
        return set(out.split())

    run_id = None
    try:
        runs = json.loads(gh(
            "run", "list", "--repo", args.repo, "--workflow", args.workflow, "--event", "workflow_dispatch",
            "--status", "success", "--limit", "100", "--json", "databaseId,headSha,createdAt",
        ))
        run_id = find_run(runs, artifacts_of, args.sha, args.require)
    except (OSError, subprocess.CalledProcessError, ValueError, KeyError) as error:
        detail = (getattr(error, "stderr", "") or str(error)).strip()
        print(f"::warning::cannot look for a dry run ({detail}); the release builds", file=sys.stderr)
    if run_id is None:
        print(f"no reusable dry run of {args.workflow} for {args.sha}: the release builds")
    else:
        print(f"reusing the artifacts of dry run {run_id} of {args.workflow}")
    line = f"run_id={'' if run_id is None else run_id}"
    print(line)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
