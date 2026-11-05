#!/usr/bin/env python3
"""Measure the shipped size of a TableX executable and check it against a budget.

  python3 scripts/size_report.py                 # measure target/dist/tablex-gpui
  python3 scripts/size_report.py --package tablex-mcp   # the standalone MCP server
  python3 scripts/size_report.py --build         # cargo build --profile dist first
  python3 scripts/size_report.py --check         # exit 1 when a budget is exceeded
  python3 scripts/size_report.py --markdown      # table for a CI job summary
  python3 scripts/size_report.py --crates 25     # largest crates (needs cargo-bloat)

Two numbers are tracked per target:
  binary      the stripped `dist` executable as it lands on disk
  compressed  the same file deflated at level 9, a stand-in for the download
              (a .dmg / .zip / installer compresses about the same)

Budgets live in scripts/size-budget.json, keyed by target (`macos-arm64`,
`linux-x86_64`, ...): the app's under `targets`, every other package's under
`packages.<name>.targets`. A target without an entry is measured and reported,
never failed. Raise a budget only in the same change that explains what grew.
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import zlib
from dataclasses import asdict, dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BUDGET_FILE = ROOT / "scripts" / "size-budget.json"
PROFILE = "dist"
PACKAGE = "tablex-gpui"
CHUNK = 1 << 20


class SizeError(Exception):
    """A problem the user can act on (missing binary, bad budget file)."""


@dataclass(frozen=True)
class Measurement:
    target: str
    binary: int
    compressed: int


@dataclass(frozen=True)
class Verdict:
    metric: str
    actual: int
    limit: int | None

    @property
    def over(self) -> bool:
        return self.limit is not None and self.actual > self.limit

    @property
    def headroom(self) -> int | None:
        return None if self.limit is None else self.limit - self.actual


def current_target() -> str:
    system = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}.get(
        platform.system(), platform.system().lower()
    )
    machine = platform.machine().lower()
    arch = {"aarch64": "arm64", "amd64": "x86_64"}.get(machine, machine)
    return f"{system}-{arch}"


def default_binary(package: str = PACKAGE) -> Path:
    name = package + (".exe" if platform.system() == "Windows" else "")
    return ROOT / "target" / PROFILE / name


def deflated_size(path: Path) -> int:
    """Bytes of `path` after raw DEFLATE at level 9, streamed."""
    compressor = zlib.compressobj(9, zlib.DEFLATED, -zlib.MAX_WBITS)
    total = 0
    with path.open("rb") as file:
        while chunk := file.read(CHUNK):
            total += len(compressor.compress(chunk))
    return total + len(compressor.flush())


def measure(binary: Path, target: str) -> Measurement:
    if not binary.is_file():
        raise SizeError(f"no executable at {binary}: build it with --build")
    return Measurement(target=target, binary=binary.stat().st_size, compressed=deflated_size(binary))


def load_budget(path: Path, target: str, package: str = PACKAGE) -> dict[str, int]:
    """The `{metric: max bytes}` of `package` on `target`, empty when it has no budget."""
    if not path.is_file():
        return {}
    try:
        budgets = json.loads(path.read_text())
    except json.JSONDecodeError as error:
        raise SizeError(f"{path} is not valid JSON: {error}") from error
    # The app's budgets are the top-level `targets`; other packages have their own.
    scope = budgets if package == PACKAGE else budgets.get("packages", {}).get(package, {})
    entry = scope.get("targets", {}).get(target, {})
    limits = {}
    for metric in ("binary", "compressed"):
        limit = entry.get(f"{metric}_max_bytes")
        if limit is None:
            continue
        if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
            raise SizeError(
                f"{path}: {package} {target}.{metric}_max_bytes must be a positive integer"
            )
        limits[metric] = limit
    return limits


def verdicts(measurement: Measurement, limits: dict[str, int]) -> list[Verdict]:
    return [
        Verdict(metric, getattr(measurement, metric), limits.get(metric))
        for metric in ("binary", "compressed")
    ]


def megabytes(size: int) -> str:
    return f"{size / 1_000_000:.2f} MB"


def render_text(measurement: Measurement, results: list[Verdict], package: str = PACKAGE) -> str:
    lines = [f"{package} ({PROFILE} profile, {measurement.target})"]
    for result in results:
        line = f"  {result.metric:<11}{megabytes(result.actual):>10}"
        if result.limit is None:
            line += "   no budget"
        else:
            state = "OVER BUDGET" if result.over else "ok"
            line += f"   budget {megabytes(result.limit)}, headroom {megabytes(result.headroom)}  {state}"
        lines.append(line)
    return "\n".join(lines)


def render_markdown(
    measurement: Measurement, results: list[Verdict], package: str = PACKAGE
) -> str:
    rows = [
        f"### {package} size ({measurement.target}, `{PROFILE}` profile)",
        "",
        "| Metric | Size | Budget | Headroom | |",
        "| --- | ---: | ---: | ---: | --- |",
    ]
    for result in results:
        budget = "–" if result.limit is None else megabytes(result.limit)
        headroom = "–" if result.headroom is None else megabytes(result.headroom)
        state = "over budget" if result.over else ("ok" if result.limit is not None else "not tracked")
        rows.append(f"| {result.metric} | {megabytes(result.actual)} | {budget} | {headroom} | {state} |")
    return "\n".join(rows)


def build(package: str = PACKAGE) -> None:
    subprocess.run(["cargo", "build", "--profile", PROFILE, "-p", package], cwd=ROOT, check=True)


def crates(count: int, package: str = PACKAGE) -> str:
    """The largest crates in the executable (cargo-bloat needs symbols)."""
    command = ["cargo", "bloat", "--profile", PROFILE, "-p", package, "--crates", "-n", str(count)]
    env = {**os.environ, "CARGO_PROFILE_DIST_STRIP": "false"}
    try:
        done = subprocess.run(command, cwd=ROOT, check=True, capture_output=True, text=True, env=env)
    except FileNotFoundError as error:
        raise SizeError("cargo is not on PATH") from error
    except subprocess.CalledProcessError as error:
        hint = "install it with `cargo install cargo-bloat`" if "no such command" in error.stderr else error.stderr
        raise SizeError(f"cargo bloat failed: {hint.strip()}") from error
    return done.stdout


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--package", default=PACKAGE, help=f"cargo package to measure (default: {PACKAGE})")
    parser.add_argument("--binary", type=Path, help="executable to measure (default: target/dist/<package>)")
    parser.add_argument("--target", help="budget key (default: this machine, e.g. macos-arm64)")
    parser.add_argument("--budget", type=Path, default=BUDGET_FILE, help="budget file")
    parser.add_argument("--build", action="store_true", help="build the dist profile first")
    parser.add_argument("--check", action="store_true", help="exit 1 when a budget is exceeded")
    output = parser.add_mutually_exclusive_group()
    output.add_argument("--json", action="store_true", help="print the measurement as JSON")
    output.add_argument("--markdown", action="store_true", help="print a Markdown table")
    parser.add_argument("--crates", type=int, metavar="N", help="also list the N largest crates")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        if args.build:
            build(args.package)
        target = args.target or current_target()
        measurement = measure(args.binary or default_binary(args.package), target)
        results = verdicts(measurement, load_budget(args.budget, target, args.package))
        if args.json:
            print(
                json.dumps(
                    {
                        "package": args.package,
                        **asdict(measurement),
                        "budget": {r.metric: r.limit for r in results},
                    }
                )
            )
        elif args.markdown:
            print(render_markdown(measurement, results, args.package))
        else:
            print(render_text(measurement, results, args.package))
        if args.crates:
            print()
            print(crates(args.crates, args.package), end="")
    except SizeError as error:
        print(f"size-report: {error}", file=sys.stderr)
        return 2
    over = [result for result in results if result.over]
    for result in over:
        print(
            f"size-report: {args.package} {result.metric} is {megabytes(-result.headroom)} over its budget "
            f"({megabytes(result.actual)} > {megabytes(result.limit)})",
            file=sys.stderr,
        )
    return 1 if args.check and over else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
