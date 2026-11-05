#!/usr/bin/env python3
"""Performance snapshot of the table Properties structure editor, to diff across commits.

  python3 scripts/perf_properties.py --output /abs/path/summary.md
  python3 scripts/perf_properties.py --output after.md --skip-process
  python3 scripts/perf_properties.py --compare before.json after.json [--output diff.md]

One run writes `<output>` (Markdown) and `<output stem>.json` (every number, for --compare), plus
`<output stem>.bench.log` (the raw benchmark output). It does, in release:

  1. the ignored benchmarks, built with the dev-only `bench` feature:
       properties_render_bench_context   frame time + allocations per frame on GPUI's headless
                                         renderer: view/edit mode x idle/scroll/commit, review
                                         dialog, SQLite, notify-loop probes (F1, F13)
       structure_edit::bench             build_rows, plan_against, name_clashes, stale, warnings,
                                         Draft::push folding, review-model cache hit vs miss
       properties_edit_retained_memory   bytes one edit mode retains / gives back, per history step
       properties_rebuild_bench          PropertiesEdit::rebuild
       properties_commit_plan_calls      plan_against calls per committed cell edit
  2. process metrics of the real app: a release build with `--features ui-scenarios`, launched
     through scripts/capture-gpui.py (table-properties-edit and table-diagram), sampled while idle
     (RSS and CPU every second, `footprint`, `vmmap --summary` at the end), and the binary size
     (scripts/size_report.py). It needs an unlocked desktop: when the desktop is locked this part
     is skipped with a note.

Benchmarks print `PERF {json}` lines; this script only collects them. Numbers come from a counting
allocator and a headless renderer: compare runs of the same machine, not absolute values.
"""
from __future__ import annotations

import argparse
import datetime
import json
import platform
import re
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CAPTURE = ROOT / "scripts" / "capture-gpui.py"
SIZE_REPORT = ROOT / "scripts" / "size_report.py"
SCENARIOS = ("table-properties-edit", "table-diagram")
BENCH_FILTERS = (
    "properties_render_bench_context",
    "structure_edit::bench",
    "properties_edit_retained_memory",
    "properties_rebuild_bench",
    "properties_commit_plan_calls",
)
# Metrics that identify a case rather than measure it (not diffed).
IDENTITY_METRICS = {"runs", "frames", "columns", "changes", "commits", "applied"}


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(command, cwd=ROOT, text=True, **kwargs)


def text_of(command: list[str]) -> str:
    try:
        done = run(command, capture_output=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return done.stdout.strip()


def environment() -> dict:
    status = text_of(["git", "status", "--porcelain"])
    return {
        "commit": text_of(["git", "rev-parse", "--short", "HEAD"]),
        "subject": text_of(["git", "log", "-1", "--format=%s"]),
        "dirty": bool(status),
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "machine": f"{platform.system()} {platform.release()} {platform.machine()}",
        "cpu": text_of(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "rustc": text_of(["rustc", "--version"]),
    }


# ---- benchmarks ----


def parse_perf(text: str) -> list[dict]:
    records = []
    for line in text.splitlines():
        if not line.startswith("PERF "):
            continue
        try:
            records.append(json.loads(line[5:]))
        except json.JSONDecodeError:
            continue
    return records


def run_benchmarks(log: Path, jobs: int) -> dict:
    command = [
        "cargo", "test", "--release", "-p", "tablex-gpui", "--features", "bench", "-j", str(jobs),
        "--", "--ignored", "--nocapture", "--test-threads=1", *BENCH_FILTERS,
    ]
    print("benchmarks: " + " ".join(command), file=sys.stderr)
    started = time.monotonic()
    with log.open("w", encoding="utf-8") as handle:
        done = run(command, stdout=handle, stderr=subprocess.STDOUT)
    text = log.read_text(encoding="utf-8", errors="replace")
    failed = re.findall(r"^CASE FAILED: (.*)$", text, flags=re.M)
    return {
        "exit_code": done.returncode,
        "seconds": round(time.monotonic() - started, 1),
        "failed_cases": failed,
        "records": parse_perf(text),
        "log": str(log),
    }


# ---- process metrics ----

UNITS = {"B": 1, "K": 1024, "KB": 1024, "M": 1024**2, "MB": 1024**2, "G": 1024**3, "GB": 1024**3, "T": 1024**4}


def to_bytes(value: str) -> int:
    match = re.fullmatch(r"([\d.]+)\s*([A-Za-z]*)", value.strip())
    if not match:
        return 0
    return int(float(match.group(1)) * UNITS.get(match.group(2).upper() or "B", 1))


def parse_cpu_time(value: str) -> float:
    """`ps -o time=` (`H:MM:SS.ss`, `MM:SS.ss`) in seconds."""
    seconds = 0.0
    for part in value.strip().split(":"):
        seconds = seconds * 60 + float(part)
    return seconds


def sample_process(pid: int, seconds: int) -> dict:
    samples = []
    start = time.monotonic()
    for tick in range(seconds):
        done = run(["ps", "-o", "rss=,%cpu=,time=", "-p", str(pid)], capture_output=True)
        fields = done.stdout.split()
        if len(fields) < 3:
            return {"error": f"the process exited after {tick} s"}
        samples.append((time.monotonic() - start, int(fields[0]), float(fields[1]), parse_cpu_time(fields[2])))
        time.sleep(max(0.0, start + tick + 1 - time.monotonic()))
    rss = [sample[1] * 1024 for sample in samples]
    wall = samples[-1][0] - samples[0][0]
    cpu_seconds = samples[-1][3] - samples[0][3]
    return {
        "samples": len(samples),
        "rss_bytes": {"first": rss[0], "last": rss[-1], "min": min(rss), "median": int(statistics.median(rss)), "max": max(rss)},
        # ps %cpu is a decaying average since launch; the cpu-time delta is the window itself.
        "cpu_percent_ps": {"median": statistics.median(s[2] for s in samples), "max": max(s[2] for s in samples)},
        "cpu_percent_window": round(100.0 * cpu_seconds / wall, 2) if wall > 0 else 0.0,
        "cpu_seconds_window": round(cpu_seconds, 2),
    }


def read_footprint(pid: int) -> dict:
    done = run(["footprint", str(pid)], capture_output=True)
    match = re.search(r"Footprint:\s*([\d.]+\s*[KMGT]?B)", done.stdout)
    if done.returncode != 0 or not match:
        return {"error": (done.stderr or done.stdout).strip().splitlines()[-1:] or "footprint failed"}
    return {"phys_footprint_bytes": to_bytes(match.group(1))}


def read_vmmap(pid: int) -> dict:
    done = run(["vmmap", "--summary", str(pid)], capture_output=True)
    if done.returncode != 0:
        return {"error": (done.stderr or done.stdout).strip().splitlines()[-1:] or "vmmap failed"}
    result = {}
    for key, pattern in (("physical_footprint_bytes", r"^Physical footprint:\s*(\S+)"), ("physical_footprint_peak_bytes", r"^Physical footprint \(peak\):\s*(\S+)")):
        match = re.search(pattern, done.stdout, flags=re.M)
        if match:
            result[key] = to_bytes(match.group(1))
    # The first TOTAL row of the region table: VIRTUAL RESIDENT DIRTY SWAPPED VOLATILE NONVOL EMPTY COUNT.
    total = re.search(r"^TOTAL\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\d+)", done.stdout, flags=re.M)
    if total:
        for name, value in zip(("virtual_bytes", "resident_bytes", "dirty_bytes", "swapped_bytes"), total.groups()):
            result[name] = to_bytes(value)
    return result


def run_scenario(binary: Path, scenario: str, seconds: int, workdir: Path) -> dict:
    output = workdir / scenario
    launch = run(
        [sys.executable, str(CAPTURE), "launch", "--binary", str(binary), "--output", str(output), "--scenario", scenario, "--fixture", "populated"],
        capture_output=True,
    )
    if launch.returncode != 0:
        message = (launch.stderr + launch.stdout).strip()
        last = message.splitlines()[-1] if message else "launch failed"
        if "desktop is locked" in message or "desktop is locked or unavailable" in message:
            return {"skipped": "desktop locked or unavailable", "locked": True, "detail": last}
        if "invalid choice" in message:
            return {"skipped": f"scenario {scenario} does not exist in this tree", "detail": last}
        return {"skipped": "launch failed", "detail": last}
    try:
        pid = int(json.loads((output / "run.json").read_text())["pid"])
        time.sleep(3)  # first frames, fixture loads
        result = {"pid": pid, **sample_process(pid, seconds)}
        result["footprint"] = read_footprint(pid)
        result["vmmap"] = read_vmmap(pid)
        return result
    finally:
        run([sys.executable, str(CAPTURE), "stop", "--output", str(output)], capture_output=True)


def run_process_metrics(workdir: Path, seconds: int, jobs: int, build: bool) -> dict:
    binary = ROOT / "target" / "release" / "tablex-gpui"
    if build:
        command = ["cargo", "build", "--release", "-p", "tablex-gpui", "--features", "ui-scenarios", "-j", str(jobs)]
        print("app build: " + " ".join(command), file=sys.stderr)
        done = run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if done.returncode != 0:
            return {"skipped": "release build failed", "detail": done.stderr.strip().splitlines()[-1:]}
    if not binary.is_file():
        return {"skipped": f"no release binary at {binary}"}
    result: dict = {"binary": str(binary)}
    size = run([sys.executable, str(SIZE_REPORT), "--binary", str(binary), "--json"], capture_output=True)
    try:
        measured = json.loads(size.stdout.strip().splitlines()[-1])
        result["size"] = {
            "binary_bytes": measured["binary"],
            "compressed_bytes": measured["compressed"],
            "note": "release profile with ui-scenarios (not the stripped LTO dist build): compare across commits only",
        }
    except (ValueError, IndexError, KeyError):
        result["size"] = {"error": (size.stderr or size.stdout).strip()}
    result["scenarios"] = {}
    for scenario in SCENARIOS:
        print(f"scenario {scenario}: {seconds} s idle sample", file=sys.stderr)
        outcome = run_scenario(binary, scenario, seconds, workdir)
        result["scenarios"][scenario] = outcome
        if outcome.get("locked"):
            for rest in SCENARIOS[SCENARIOS.index(scenario) + 1:]:
                result["scenarios"][rest] = {"skipped": outcome["skipped"]}
            break
    return result


# ---- report ----


def mb(value: float) -> str:
    return f"{value / 1024 / 1024:.2f}"


def table(headers: list[str], rows: list[list[str]]) -> list[str]:
    if not rows:
        return ["_no data_", ""]
    lines = ["| " + " | ".join(headers) + " |", "|" + "|".join(" --- " if i == 0 else " ---: " for i in range(len(headers))) + "|"]
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    return lines + [""]


def pick(records: list[dict], group: str) -> list[dict]:
    return [record for record in records if record["group"] == group]


def number(record: dict, key: str, digits: int = 2) -> str:
    value = record["metrics"].get(key)
    return "-" if value is None else (f"{value:,.0f}" if digits == 0 else f"{value:,.{digits}f}")


def render_markdown(summary: dict) -> str:
    env = summary["environment"]
    bench = summary.get("bench", {})
    records = bench.get("records", [])
    out = [
        "# Properties structure editor: performance snapshot",
        "",
        f"- commit `{env['commit']}`{' (+ uncommitted changes)' if env['dirty'] else ''}: {env['subject']}",
        f"- {env['date']} on {env['machine']} ({env['cpu']}), {env['rustc']}",
    ]
    if bench:
        out.append(f"- benchmarks: exit code {bench['exit_code']}, {bench['seconds']} s, {len(records)} results" + (f", FAILED CASES: {bench['failed_cases']}" if bench["failed_cases"] else ""))
    out.append("")

    # Findings, by tag.
    tagged = [record for record in records if re.match(r"F\d", record["case"])]
    if tagged:
        out += ["## Review findings (F1..F13)", ""]
        rows = []
        for record in sorted(tagged, key=lambda r: (int(re.match(r"F(\d+)", r["case"]).group(1)), r["group"], r["case"])):
            m = record["metrics"]
            if record["group"] == "quiet":
                detail = f"{m['renders']:.0f} renders (max {m['max_renders']:.0f}), {m['rebuilds']:.0f} rebuilds, {m['plan_against']:.0f} plan_against, {m['catalog_loads']:.0f} catalog loads in {m['seconds']:.2f} s after one notify"
            elif record["group"] == "render":
                detail = f"p50 {m['p50_ms']:.2f} ms, p95 {m['p95_ms']:.2f}, {m['allocs_per_frame']:,.0f} allocs/frame, {m['kib_per_frame']:,.0f} KiB/frame, {m['renders_per_frame']:.2f} renders/frame, {m['rebuilds_per_frame']:.2f} rebuilds/frame, {m['catalog_loads_per_frame']:.2f} loads/frame"
            elif record["group"] == "commit":
                detail = f"commit p50 {m['commit_p50_ms']:.2f} ms, {m['commit_allocs']:,.0f} allocs, {m['commit_rebuilds']:.2f} rebuilds, {m['commit_plan_against']:.2f} plan_against per commit"
            elif record["group"] == "commit_plan_calls":
                detail = f"{m['plan_against_per_commit']:.2f} plan_against, {m['rebuilds_per_commit']:.2f} rebuilds, {m.get('renders_per_commit', 0):.1f} renders per commit (test platform)"
            elif record["group"] == "rebuild":
                detail = f"rebuild median {m['median_us']:,.0f} us, {m['allocs']:,.0f} allocs, {m['bytes'] / 1024:,.0f} KiB"
            else:
                detail = ", ".join(f"{k}={v:,.2f}" for k, v in m.items())
            rows.append([record["case"], record["group"], detail])
        out += table(["case", "kind", "result"], rows)

    out += ["## Frames (headless Metal, real AppRoot)", ""]
    rows = []
    for record in pick(records, "render"):
        rows.append([record["case"], number(record, "frames", 0), number(record, "p50_ms", 3), number(record, "p95_ms", 3), number(record, "max_ms", 3),
                     number(record, "allocs_per_frame", 0), number(record, "kib_per_frame", 1), number(record, "renders_per_frame"),
                     number(record, "rebuilds_per_frame"), number(record, "plans_per_frame"), number(record, "catalog_loads_per_frame")])
    out += table(["case", "frames", "p50 ms", "p95 ms", "max ms", "allocs/frame", "KiB/frame", "renders/frame", "rebuilds/frame", "plan_against/frame", "catalog loads/frame"], rows)

    out += ["## Inline cell edit commit (the commit call itself)", ""]
    rows = [[r["case"], number(r, "commits", 0), number(r, "applied", 0), number(r, "commit_p50_ms", 3), number(r, "commit_p95_ms", 3), number(r, "commit_max_ms", 3),
             number(r, "commit_allocs", 0), number(r, "commit_kib", 1), number(r, "commit_rebuilds"), number(r, "commit_plan_against")] for r in pick(records, "commit")]
    out += table(["case", "commits", "applied", "p50 ms", "p95 ms", "max ms", "allocs", "KiB", "rebuilds", "plan_against"], rows)

    out += ["## Notify-loop probes (one notify, then 1 s of pumping)", ""]
    rows = [[r["case"], number(r, "runs", 0), number(r, "renders", 0), number(r, "max_renders", 0), number(r, "rebuilds", 0), number(r, "plan_against", 0), number(r, "catalog_loads", 0), number(r, "allocs", 0), number(r, "kib", 0)]
            for r in pick(records, "quiet")]
    out += table(["case", "runs", "renders", "max renders", "rebuilds", "plan_against", "catalog loads", "allocs", "KiB"], rows)

    micro_groups = ("build_rows", "plan_against", "name_clashes", "stale", "warnings", "draft_push", "session_edit", "review_cache", "rebuild", "undo_redo")
    out += ["## Micro benchmarks (median of repeated runs)", ""]
    rows = []
    for group in micro_groups:
        for record in pick(records, group):
            per_edit = record["metrics"].get("per_edit_us")
            rows.append([group, record["case"], number(record, "median_us", 1), number(record, "min_us", 1), "-" if per_edit is None else f"{per_edit:.2f}", number(record, "allocs", 0), number(record, "bytes", 0)])
    out += table(["bench", "case", "median us", "min us", "per edit us", "allocs", "bytes"], rows)
    out += [
        "Note: undo and redo turn a history snapshot back into a draft, which deep-copies every change (O(K)); so does the first edit",
        "of a draft the session did not make itself (`Snapshot::of`). An edit that follows only copies the changes it touched.",
        "The `undo_redo` rows measure both (first edit, undo, redo) at K=20, 200 and 2000.",
        "",
    ]

    out += ["## Retained memory (live heap bytes)", ""]
    out += table(["measurement", "bytes", "KiB"], [[r["case"], number(r, "bytes", 0), f"{r['metrics']['bytes'] / 1024:,.1f}"] for r in pick(records, "retained")])

    out += ["## plan_against per committed cell edit (test platform, includes its frames)", ""]
    rows = [[r["case"], number(r, "plan_against_per_commit"), number(r, "rebuilds_per_commit"), number(r, "renders_per_commit", 1), number(r, "allocs_per_commit", 0), number(r, "kib_per_commit", 0)] for r in pick(records, "commit_plan_calls")]
    out += table(["case", "plan_against", "rebuilds", "renders", "allocs", "KiB"], rows)

    process = summary.get("process")
    out += ["## Process metrics (release build, ui-scenarios, idle)", ""]
    if not process:
        out += ["_not run_", ""]
    elif "skipped" in process:
        out += [f"_skipped: {process['skipped']}_ {process.get('detail', '')}", ""]
    else:
        size = process.get("size", {})
        if "binary_bytes" in size:
            out.append(f"- binary size: {mb(size['binary_bytes'])} MiB, compressed {mb(size['compressed_bytes'])} MiB ({size['note']})")
        out.append("")
        rows = []
        notes = []
        for name, scenario in process.get("scenarios", {}).items():
            if "skipped" in scenario:
                notes.append(f"- `{name}`: skipped, {scenario['skipped']}" + (f" ({scenario['detail']})" if scenario.get("detail") else ""))
                continue
            if "error" in scenario:
                notes.append(f"- `{name}`: {scenario['error']}")
                continue
            rss, foot, vm = scenario["rss_bytes"], scenario.get("footprint", {}), scenario.get("vmmap", {})
            rows.append([name, str(scenario["samples"]), mb(rss["first"]), mb(rss["last"]), mb(rss["median"]), mb(rss["max"]),
                         f"{scenario['cpu_percent_window']:.2f}", f"{scenario['cpu_percent_ps']['max']:.1f}",
                         mb(foot["phys_footprint_bytes"]) if "phys_footprint_bytes" in foot else "-",
                         mb(vm["physical_footprint_bytes"]) if "physical_footprint_bytes" in vm else "-",
                         mb(vm["resident_bytes"]) if "resident_bytes" in vm else "-",
                         mb(vm["dirty_bytes"]) if "dirty_bytes" in vm else "-",
                         mb(vm["virtual_bytes"]) if "virtual_bytes" in vm else "-"])
        out += table(["scenario", "samples", "RSS first MiB", "RSS last", "RSS median", "RSS max", "CPU % (window)", "ps %cpu max", "footprint MiB", "vmmap phys footprint", "vmmap resident", "vmmap dirty", "vmmap virtual"], rows)
        out += notes + [""] if notes else []
    return "\n".join(out) + "\n"


def key_metrics(record: dict) -> dict:
    return {key: value for key, value in record["metrics"].items() if key not in IDENTITY_METRICS and isinstance(value, (int, float))}


def compare(before_path: Path, after_path: Path) -> str:
    before = json.loads(before_path.read_text())
    after = json.loads(after_path.read_text())
    index = {(r["group"], r["case"]): r for r in before.get("bench", {}).get("records", [])}
    rows = []
    for record in after.get("bench", {}).get("records", []):
        old = index.get((record["group"], record["case"]))
        if old is None:
            continue
        old_metrics = key_metrics(old)
        for name, value in key_metrics(record).items():
            if name not in old_metrics:
                continue
            was = old_metrics[name]
            change = "-" if was == 0 else f"{(value - was) / was * 100:+.1f}%"
            rows.append([record["group"], record["case"], name, f"{was:,.3f}", f"{value:,.3f}", change])
    out = [
        f"# Diff: {before['environment']['commit']} -> {after['environment']['commit']}",
        "",
    ]
    out += table(["bench", "case", "metric", "before", "after", "change"], rows)
    for label, summary in (("before", before), ("after", after)):
        for name, scenario in (summary.get("process") or {}).get("scenarios", {}).items():
            if "rss_bytes" in scenario:
                out.append(f"- {label} {name}: RSS median {mb(scenario['rss_bytes']['median'])} MiB, CPU {scenario['cpu_percent_window']:.2f}%, footprint {mb(scenario.get('footprint', {}).get('phys_footprint_bytes', 0))} MiB")
    return "\n".join(out) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--output", type=Path, help="Markdown summary to write (absolute path); the JSON goes next to it")
    parser.add_argument("--compare", nargs=2, type=Path, metavar=("BEFORE_JSON", "AFTER_JSON"), help="diff two JSON summaries instead of measuring")
    parser.add_argument("--skip-bench", action="store_true", help="skip the cargo benchmarks")
    parser.add_argument("--skip-process", action="store_true", help="skip the running-app measurements")
    parser.add_argument("--skip-build", action="store_true", help="measure the release binary that is already built")
    parser.add_argument("--sample-seconds", type=int, default=30, help="idle sampling window per scenario (default 30)")
    parser.add_argument("--jobs", type=int, default=6, help="cargo -j (default 6)")
    args = parser.parse_args(argv)

    if args.compare:
        report = compare(*args.compare)
        if args.output:
            args.output.write_text(report, encoding="utf-8")
        print(report)
        return 0
    if not args.output:
        parser.error("--output is required")
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    summary: dict = {"environment": environment()}
    if not args.skip_bench:
        summary["bench"] = run_benchmarks(output.with_suffix(".bench.log"), args.jobs)
    if not args.skip_process:
        with tempfile.TemporaryDirectory(prefix="tablex-perf-") as workdir:
            summary["process"] = run_process_metrics(Path(workdir), args.sample_seconds, args.jobs, not args.skip_build)
    output.with_suffix(".json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    report = render_markdown(summary)
    output.write_text(report, encoding="utf-8")
    print(report)
    failed = summary.get("bench", {}).get("exit_code", 0) != 0
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
