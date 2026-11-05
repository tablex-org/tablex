#!/usr/bin/env python3
"""Small native macOS driver for the GPUI UI scenario binary."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any


ROOT = Path(__file__).resolve().parent
HELPER_SOURCE = ROOT / "gpui-capture-helper.swift"
HELPER_PREFIX = "tablex-gpui-capture-helper-"
SCENARIOS = ("new-connection", "connection-tree", "connection-overview", "result-grid", "result-footer", "mongo-grid", "table-diagram", "database-diagram", "table-properties-edit", "driver-manager", "driver-manager-tools", "missing-driver", "split-panes", "split-panes-drop", "grouped-tabs", "grouped-tabs-lone", "settings", "settings-shortcuts", "connection-overrides", "import-selection", "redis", "assistant", "assistant-streaming", "assistant-no-key", "assistant-access-off", "assistant-error", "assistant-approval", "assistant-approval-dangerous", "assistant-approval-long", "assistant-access-write", "assistant-history", "assistant-readonly", "assistant-editor-action", "settings-ai", "settings-ai-ollama")
FIXTURES = ("sample", "populated", "pending-edits", "empty", "loading", "error", "narrow", "disconnected", "states")
THEMES = ("dark", "light", "system")
DEFAULT_SCENARIO = "new-connection"
DEFAULT_FIXTURE = "sample"
EVENTS = {"ready", "ui-state", "ui-bounds", "test-start", "test-success", "test-error", "save"}
TERMINAL_EVENTS = {"ready", "test-success", "test-error", "save"}
# Emitted only by `--features ui-scenarios,perf-diagnostics` builds.
HANG_EVENT = "hang"
KNOWN_SELECTED_NODES = {
    "none",
    "connection:11111111-1111-4111-8111-111111111111",
    "connection:22222222-2222-4222-8222-222222222222",
    "connection:33333333-3333-4333-8333-333333333333",
    "database:22222222-2222-4222-8222-222222222222/warehouse",
    "schema:22222222-2222-4222-8222-222222222222/warehouse/public",
    "object:22222222-2222-4222-8222-222222222222/warehouse/public/orders",
    'Connection { connection: "11111111-1111-4111-8111-111111111111" }',
    'Connection { connection: "22222222-2222-4222-8222-222222222222" }',
    'Connection { connection: "33333333-3333-4333-8333-333333333333" }',
    'Database { connection: "22222222-2222-4222-8222-222222222222", database: "warehouse" }',
    'Schema { connection: "22222222-2222-4222-8222-222222222222", database: "warehouse", schema: "public" }',
    'Object { connection: "22222222-2222-4222-8222-222222222222", database: "warehouse", schema: "public", object: "orders" }',
}
KNOWN_ACTIVE_TABS = {"none", "orders", "Track", "main diagram", "daily_summary", "long_view_name_東京", "events", "users", "audit_log", "long_collection_東京"} | {f"fixture_table_{index:02}" for index in range(1, 20)} | {f"fixture_collection_{index:02}" for index in range(25)}
IMAGE_INVENTORY_KEY = "owned_images"
CLICK_TARGET_STEPS = {
    "connection-picker-next": "picker",
    "connection-config-type-control": "config",
}


class RunnerError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise RunnerError(message)


def absolute_output(value: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        fail(f"--output must be an absolute path: {value}")
    return path


def safe_case(value: str) -> str:
    if not value or value in {".", ".."} or Path(value).name != value:
        fail(f"unsafe case name: {value!r}")
    if any(char not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for char in value):
        fail(f"unsafe case name: {value!r}")
    return value


def scenario_title(scenario: str) -> str:
    if scenario not in SCENARIOS:
        fail(f"unsupported scenario: {scenario}")
    return f"TableX UI Scenario: {scenario}"


def binary_path(value: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        fail(f"--binary must be an absolute path: {value}")
    path = Path(os.path.realpath(path))
    if not path.is_file() or not os.access(path, os.X_OK):
        fail(f"binary is not an executable file: {path}")
    return path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent, text=True)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def append_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        json.dump(value, handle, separators=(",", ":"))
        handle.write("\n")
        handle.flush()


def helper_binary() -> Path:
    source = HELPER_SOURCE.read_bytes()
    digest = hashlib.sha256(source).hexdigest()[:16]
    target = Path(tempfile.gettempdir()) / f"{HELPER_PREFIX}{digest}"
    if target.is_file() and os.access(target, os.X_OK):
        return target
    swiftc = "swiftc"
    command = [
        swiftc,
        "-O",
        "-framework",
        "AppKit",
        "-framework",
        "ApplicationServices",
        "-framework",
        "CoreGraphics",
        str(HELPER_SOURCE),
        "-o",
        str(target),
    ]
    try:
        completed = subprocess.run(command, text=True, capture_output=True, check=False)
    except OSError as error:
        fail(f"could not compile Swift helper with swiftc: {error}")
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        fail(f"could not compile Swift helper: {detail}")
    return target


def helper(*arguments: str, stdin: str | None = None) -> Any:
    command = [str(helper_binary()), *arguments]
    try:
        completed = subprocess.run(command, input=stdin, text=True, capture_output=True, check=False)
    except OSError as error:
        fail(f"could not run Swift helper: {error}")
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        fail(detail or f"Swift helper failed with status {completed.returncode}")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        fail(f"Swift helper returned invalid JSON: {error}")


def load_run(output: Path) -> dict[str, Any]:
    path = output / "run.json"
    if not path.is_file():
        fail(f"run state does not exist: {path}")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read {path}: {error}")
    if not isinstance(value, dict):
        fail(f"invalid run state: {path}")
    return value


def require_desktop(*, capture: bool = False, input_events: bool = False) -> None:
    state = helper("environment")
    if not state.get("desktop_available") or state.get("locked"):
        fail("desktop is locked or unavailable; unlock macOS before UI automation")
    if capture and not state.get("capture_allowed"):
        fail("Screen Recording permission is required for capture")
    if input_events and not state.get("input_allowed"):
        fail("Accessibility/post-event permission is required for input")


def read_events(output: Path) -> list[dict[str, Any]]:
    path = output / "stdout.log"
    if not path.is_file():
        return []
    events: list[dict[str, Any]] = []
    try:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict) and value.get("event") in EVENTS:
                    events.append(value)
    except OSError as error:
        fail(f"could not read app stdout log: {error}")
    return events


def read_hang_incidents(output: Path) -> list[dict[str, Any]]:
    """Hang incidents a `perf-diagnostics` build wrote to the app stdout log.

    Kept out of `read_events` on purpose: that list drives checkpoint ordering
    (`events[-1]`), and a hang line landing after `ready` must not hide it.
    """
    path = output / "stdout.log"
    if not path.is_file():
        return []
    incidents: list[dict[str, Any]] = []
    try:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict) and value.get("event") == HANG_EVENT and isinstance(value.get("incident"), dict):
                    incidents.append(value)
    except OSError as error:
        fail(f"could not read app stdout log: {error}")
    return incidents


def process_identity(run: dict[str, Any]) -> tuple[int, dict[str, Any]]:
    try:
        pid = int(run["pid"])
        expected_path = str(run["binary_realpath"])
        expected_hash = str(run["binary_sha256"])
    except (KeyError, TypeError, ValueError) as error:
        fail(f"invalid run identity: {error}")
    if pid <= 0:
        fail(f"invalid run pid: {pid}")
    info = helper("process-info", "--pid", str(pid))
    if not isinstance(info, dict) or not info.get("alive"):
        fail(f"owned TableX process is not running: pid {pid}")
    actual_path = os.path.realpath(str(info.get("path", "")))
    if actual_path != expected_path:
        fail(f"pid {pid} is not owned by this runner (path {actual_path!r})")
    if sha256(Path(expected_path)) != expected_hash:
        fail(f"binary changed since launch: {expected_path}")
    stored_launch = run.get("launch_time")
    current_launch = info.get("launch_time")
    if stored_launch is not None and current_launch is not None:
        if abs(float(stored_launch) - float(current_launch)) > 0.5:
            fail(f"pid {pid} is not the process launched by this runner")
    return pid, info


def windows_for(run: dict[str, Any]) -> dict[str, Any]:
    pid, _ = process_identity(run)
    title = scenario_title(str(run.get("scenario", DEFAULT_SCENARIO)))
    if run.get("window_title", title) != title:
        fail("run window title does not match its recorded scenario")
    windows = helper(
        "list-windows",
        "--pid",
        str(pid),
        "--path",
        str(run["binary_realpath"]),
        "--title",
        title,
    )
    if not isinstance(windows, list):
        fail("Swift helper returned an invalid window list")
    wanted_id = ((run.get("window") or {}).get("id"))
    candidates = [window for window in windows if isinstance(window, dict)]
    current = next((window for window in candidates if window.get("id") == wanted_id), None)
    if current is None:
        current = next(
            (window for window in candidates if window.get("title") == title),
            None,
        )
    if current is None:
        fail(f"could not find owned TableX window for pid {pid}")
    if current.get("title") != title:
        fail(f"owned window has unexpected title: {current.get('title')!r}")
    return current


def update_window(output: Path, run: dict[str, Any], *, persist: bool = True) -> dict[str, Any]:
    window = windows_for(run)
    run["window"] = window
    run["window_id"] = window["id"]
    run["window_bounds"] = window["bounds"]
    if persist:
        write_json(output / "run.json", run)
    return window


def launch_env(theme: str | None) -> dict[str, str]:
    """Scenario runs force the theme with TABLEX_THEME (dark|light|system)."""
    env = os.environ.copy()
    if theme:
        env["TABLEX_THEME"] = theme
    return env


def launch(args: argparse.Namespace) -> None:
    require_desktop(capture=True, input_events=True)
    output = absolute_output(args.output)
    if output.is_symlink():
        fail(f"--output must not be a symlink: {output}")
    if output.exists():
        if not output.is_dir():
            fail(f"--output is not a directory: {output}")
        if any(output.iterdir()):
            fail(f"--output must be a fresh empty directory; choose a new output path: {output}")
    else:
        output.mkdir(parents=True)
    output_realpath = Path(os.path.realpath(output))
    binary = binary_path(args.binary)
    digest = sha256(binary)
    scenario = getattr(args, "scenario", DEFAULT_SCENARIO)
    fixture = getattr(args, "fixture", DEFAULT_FIXTURE)
    title = scenario_title(scenario)
    if fixture not in FIXTURES:
        fail(f"unsupported fixture: {fixture}")
    old = output / "run.json"
    if old.is_file():
        try:
            previous = json.loads(old.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            previous = None
        if isinstance(previous, dict):
            try:
                process_identity(previous)
            except RunnerError:
                pass
            else:
                fail(f"an owned TableX process is already running in {output}")

    stdout_path = output / "stdout.log"
    (output / "actions.jsonl").write_text("", encoding="utf-8")
    with stdout_path.open("w", encoding="utf-8") as log:
        try:
            process = subprocess.Popen(
                [
                    str(binary),
                    "--ui-scenario",
                    scenario,
                    "--ui-fixture",
                    fixture,
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
                env=launch_env(getattr(args, "theme", None)),
                start_new_session=True,
            )
        except OSError as error:
            fail(f"could not launch {binary}: {error}")
    run: dict[str, Any] = {}
    try:
        launch_info = helper("process-info", "--pid", str(process.pid))
        if not launch_info.get("alive") or os.path.realpath(str(launch_info.get("path", ""))) != str(binary):
            fail(f"launched pid {process.pid} did not resolve to {binary}")
        run = {
            "status": "starting",
            "pid": process.pid,
            "binary": str(binary),
            "binary_realpath": str(binary),
            "binary_sha256": digest,
            "launch_time": launch_info.get("launch_time"),
            "started_at": time.time(),
            "output_realpath": str(output_realpath),
            "scenario": scenario,
            "fixture": fixture,
            "stdout_log": str(stdout_path),
            "window_title": title,
            IMAGE_INVENTORY_KEY: [],
            "event_count": len(read_events(output)),
        }
        write_json(output / "run.json", run)
        window_deadline = time.monotonic() + args.window_timeout
        while True:
            try:
                run["window"] = windows_for(run)
                run["window_id"] = run["window"]["id"]
                run["window_bounds"] = run["window"]["bounds"]
                break
            except RunnerError:
                if process.poll() is not None or time.monotonic() >= window_deadline:
                    fail(f"timed out finding owned TableX window; see {stdout_path}")
                time.sleep(0.1)
        # An inactive native window may not paint. Activate the owned window before awaiting its rendered checkpoint.
        helper(
            "activate",
            "--pid",
            str(process.pid),
            "--path",
            str(binary),
            "--title",
            title,
            "--window-id",
            str(run["window_id"]),
        )
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            events = read_events(output)
            if any(event.get("event") == "ready" and event.get("pid") == process.pid for event in events):
                break
            if process.poll() is not None:
                fail(f"TableX exited before ready; see {stdout_path}")
            time.sleep(0.05)
        else:
            fail(f"timed out waiting for ready event; see {stdout_path}")
        run["status"] = "running"
        run["event_count"] = len(events)
        for event in reversed(events):
            if (
                event.get("event") == "ui-state"
                and event.get("pid") == process.pid
                and isinstance(event.get("window_id"), int)
            ):
                run["initial_ui_window_id"] = event["window_id"]
                break
        write_json(output / "run.json", run)
        print(json.dumps(run, indent=2, sort_keys=True))
    except BaseException:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if run:
            run["status"] = "failed"
            try:
                write_json(output / "run.json", run)
            except OSError:
                pass
        raise


def validate_coordinate(value: float, name: str) -> float:
    if not math.isfinite(value):
        fail(f"{name} must be finite")
    return value


def expected_state(items: list[str]) -> dict[str, Any]:
    choices = {
        "step": {"picker", "config"},
        "category": {"Relational", "Analytics", "Chinese DBs", "Lightweight", "Docs/Cache/Search", "Graph/Vector", "Time series", "Message queues", "Registry/Config"},
        "view": {"Grid", "List"},
        "kind": {"PostgreSQL", "MongoDB", "SQLite"},
        "next_enabled": {"true", "false"},
        "use_uri": {"true", "false"},
        "inline_error": {"true", "false"},
        "query_status": {"Idle", "Running", "Ready", "Failed", "Cancelled"},
        "multi_mode": {"true", "false"},
        "result_source": {"bson", "arrow"},
        "active_schema": {"public", "analytics", "main"},
        "active_connection": {"11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333"},
    }
    result: dict[str, Any] = {}
    for item in items:
        field, separator, value = item.partition("=")
        if not separator or field in result:
            fail("--expect requires distinct FIELD=VALUE pairs")
        if field == "search_length":
            if not value.isascii() or not value.isdigit() or not 0 <= int(value) <= 100_000:
                fail("expected search_length must be an integer from 0 to 100000")
            result[field] = int(value)
        elif field == "column_widths":
            try:
                widths = json.loads(value)
            except (ValueError, TypeError):
                fail("expected column_widths must be a numeric JSON array")
            if not isinstance(widths, list) or len(widths) > 256 or not all(type(width) in (int, float) and math.isfinite(width) and 16 <= width <= 4096 for width in widths):
                fail("expected column_widths must contain finite pixel widths from 16 to 4096")
            result[field] = widths
        elif field in {"page_rows", "page_offset", "total_rows", "page_generation", "query_generation", "first_row_id", "pending_count", "deleted_count", "expanded_nodes", "visible_nodes", "loading_nodes", "selection_count"}:
            if not value.isascii() or not value.isdigit() or not 0 <= int(value) <= 1_000_000_000:
                fail(f"expected {field} must be a non-negative integer")
            result[field] = int(value)
        elif field == "first_document_id" and len(value) == 24 and all(char in "0123456789abcdef" for char in value):
            result[field] = value
        elif field == "active_tab" and value in KNOWN_ACTIVE_TABS:
            result[field] = None if value == "none" else value
        elif field == "selected_node" and value in KNOWN_SELECTED_NODES:
            result[field] = None if value == "none" else value
        elif field in choices and value in choices[field]:
            result[field] = value == "true" if field in {"next_enabled", "use_uri", "inline_error", "multi_mode"} else value
        else:
            fail("unsupported observation field or value; credentials are never observable")
    return result


def await_ui_state(output: Path, pid: int, baseline: int, expected: dict[str, Any], timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        fresh = [event for event in read_events(output)[baseline:] if event.get("event") == "ui-state" and event.get("pid") == pid]
        state = fresh[-1].get("state", {}) if fresh else {}
        if fresh and isinstance(state, dict) and all(state.get(key) == value for key, value in expected.items()):
            return state
        time.sleep(0.05)
    fail("input submitted but no fresh matching UI state was observed before timeout")


def latest_dialog_state(events: list[dict[str, Any]], pid: int) -> dict[str, Any]:
    latest = None
    for event in events:
        if not isinstance(event, dict) or event.get("event") != "ui-state" or event.get("pid") != pid:
            continue
        if event.get("closed") is True:
            if isinstance(event.get("dialog_id"), int) and isinstance(event.get("window_id"), int):
                latest = {
                    "closed": True,
                    "dialog_id": event["dialog_id"],
                    "window_id": event["window_id"],
                }
            continue
        if (
            isinstance(event.get("state"), dict)
            and isinstance(event.get("dialog_id"), int)
            and isinstance(event.get("window_id"), int)
        ):
            latest = dict(event["state"])
            latest["dialog_id"] = event["dialog_id"]
            latest["window_id"] = event["window_id"]
    if latest is not None:
        return latest
    fail("no fresh current dialog state was reported by the owned process")


def current_content_bounds(window: dict[str, Any]) -> dict[str, float]:
    try:
        current_width = float(window["bounds"]["width"])
        current_height = float(window["bounds"]["height"])
    except (KeyError, TypeError, ValueError):
        fail("owned window geometry is invalid for semantic click")
    if (
        not all(math.isfinite(value) for value in (current_width, current_height))
        or current_width <= 0
        or current_height <= 0
    ):
        fail("current owned window geometry is invalid for semantic click")
    return {"width": current_width, "height": current_height}


def resolve_click_id(
    events: list[dict[str, Any]],
    *,
    pid: int,
    target: str,
    window: dict[str, Any],
    current_state: dict[str, Any],
) -> dict[str, Any]:
    expected_step = CLICK_TARGET_STEPS.get(target)
    if expected_step is None:
        fail(f"unsupported click target: {target}")
    window_bounds = window.get("content_bounds")
    if not isinstance(window_bounds, dict):
        fail("current owned window geometry is unavailable for semantic click")
    try:
        window_width = float(window_bounds["width"])
        window_height = float(window_bounds["height"])
    except (KeyError, TypeError, ValueError):
        fail("owned window bounds are invalid for semantic click")
    current_step = current_state.get("step")
    if current_step != expected_step:
        fail(f"click target {target} is stale for current step")
    current_dialog_id = current_state.get("dialog_id")
    current_window_id = current_state.get("window_id", window.get("ui_window_id"))
    if not isinstance(current_dialog_id, int) or not isinstance(current_window_id, int):
        fail("current dialog identity is unavailable for semantic click")
    candidates = [
        event
        for event in events
        if isinstance(event, dict) and event.get("event") == "ui-bounds" and event.get("target") == target
    ]
    if not candidates:
        fail(f"no rendered bounds were reported for click target {target}")
    owned = [event for event in candidates if event.get("pid") == pid]
    if not owned:
        fail(f"rendered bounds for {target} do not belong to the owned pid")
    event = owned[-1]
    if event.get("dialog_id") != current_dialog_id:
        fail(f"click target {target} is stale for the current dialog")
    if event.get("window_id") != current_window_id:
        fail(f"click target {target} is stale for the current window")
    if event.get("step") != current_step:
        fail(f"click target {target} is stale for current step")
    if event.get("enabled") is not True:
        fail(f"click target {target} is disabled")
    if event.get("visible") is not True:
        fail(f"click target {target} is not visible")
    bounds = event.get("bounds")
    if not isinstance(bounds, dict):
        fail(f"rendered bounds for {target} are invalid")
    try:
        x, y, width, height = (float(bounds[key]) for key in ("x", "y", "width", "height"))
    except (KeyError, TypeError, ValueError):
        fail(f"rendered bounds for {target} are invalid")
    values = (x, y, width, height)
    if not all(math.isfinite(value) for value in values) or width <= 0 or height <= 0 or x < 0 or y < 0:
        fail(f"rendered bounds for {target} are invalid")
    if x + width > window_width or y + height > window_height:
        fail(f"rendered bounds for {target} exceed the owned window")
    try:
        rendered_width = float(event["window_width"])
        rendered_height = float(event["window_height"])
    except (KeyError, TypeError, ValueError):
        fail(f"rendered window bounds for {target} are unavailable")
    if (
        not math.isfinite(rendered_width)
        or not math.isfinite(rendered_height)
        or rendered_width <= 0
        or rendered_height <= 0
        or not math.isclose(rendered_width, window_width, abs_tol=0.01)
        or not math.isclose(rendered_height, window_height, abs_tol=0.01)
        or x + width > rendered_width
        or y + height > rendered_height
    ):
        fail(f"rendered bounds for {target} exceed the current window geometry")
    return {"x": x + width / 2, "y": y + height / 2, "bounds": {key: bounds[key] for key in ("x", "y", "width", "height")}}


def act(args: argparse.Namespace) -> None:
    click_modifier = getattr(args, "click_modifier", None)
    if click_modifier is not None and (getattr(args, "click", None) is None and getattr(args, "click_id", None) is None):
        fail("--click-modifier is supported only with --click or --click-id")
    require_desktop(input_events=True)
    expectation = expected_state(args.expect)
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        fail("--timeout must be finite and positive")
    output = absolute_output(args.output)
    run = load_run(output)
    pid, _ = process_identity(run)
    previous_native_window_id = run.get("window_id")
    window = update_window(output, run)
    if previous_native_window_id is not None and window.get("id") != previous_native_window_id:
        fail("owned native window identity changed before semantic click")
    title = scenario_title(str(run.get("scenario", DEFAULT_SCENARIO)))
    if run.get("window_title", title) != title:
        fail("run window title does not match its recorded scenario")
    events_before = read_events(output)
    event_count_before = len(events_before)
    action: dict[str, Any]
    double_click = getattr(args, "double_click", None)
    right_click = getattr(args, "right_click", None)
    drag = getattr(args, "drag", None)
    move = getattr(args, "move", None)
    click_id = getattr(args, "click_id", None)
    if click_id is not None:
        current_state = latest_dialog_state(events_before, pid)
        if current_state.get("closed") is True:
            fail("current connection dialog is closed")
        initial_ui_window_id = run.get("initial_ui_window_id")
        if initial_ui_window_id is not None and current_state["window_id"] != initial_ui_window_id:
            fail("owned UI window identity changed before semantic click")
        window["ui_window_id"] = current_state["window_id"]
        window["content_bounds"] = current_content_bounds(window)
        resolved = resolve_click_id(
            events_before,
            pid=pid,
            target=click_id,
            window=window,
            current_state=current_state,
        )
        x, y = resolved["x"], resolved["y"]
        helper(
            "click",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            "--x",
            str(x),
            "--y",
            str(y),
            "--click-count",
            "1",
            *(["--modifier", click_modifier] if click_modifier else []),
        )
        action = {"kind": "click-id", "id": click_id, "x": x, "y": y, "bounds": resolved["bounds"]}
        if click_modifier:
            action["modifier"] = click_modifier
    elif args.click is not None or double_click is not None or right_click is not None:
        coordinates = args.click if args.click is not None else double_click if double_click is not None else right_click
        x, y = (validate_coordinate(float(value), name) for value, name in zip(coordinates, ("x", "y")))
        helper(
            "right-click" if right_click is not None else "click",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            "--x",
            str(x),
            "--y",
            str(y),
            "--click-count",
            "2" if double_click is not None else "1",
            *(["--modifier", click_modifier] if click_modifier else []),
        )
        action = {
            "kind": "right-click" if right_click is not None else "double-click" if double_click is not None else "click",
            "x": x,
            "y": y,
        }
        if click_modifier:
            action["modifier"] = click_modifier
    elif drag is not None:
        x1, y1, x2, y2 = (
            validate_coordinate(float(value), name)
            for value, name in zip(drag, ("x1", "y1", "x2", "y2"))
        )
        helper(
            "drag",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            "--x1",
            str(x1),
            "--y1",
            str(y1),
            "--x2",
            str(x2),
            "--y2",
            str(y2),
        )
        action = {"kind": "drag", "x1": x1, "y1": y1, "x2": x2, "y2": y2}
    elif move is not None:
        if not 1 <= args.move_hz <= 120:
            fail("--move-hz must be an integer from 1 to 120")
        if not 1 <= len(move) <= 4096:
            fail("provide between 1 and 4096 --move points")
        points = [
            [validate_coordinate(float(x), "x"), validate_coordinate(float(y), "y")]
            for x, y in move
        ]
        result = helper(
            "mouse-move-trace",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            stdin=json.dumps({"points": points, "hz": args.move_hz}),
        )
        action = {
            "kind": "mouse-move-trace",
            "points": len(points),
            "hz": args.move_hz,
            "helper_timing": result,
        }
    elif args.text is not None:
        helper(
            "text",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            "--text-stdin",
            stdin=args.text,
        )
        action = {"kind": "text", "length": len(args.text)}
    else:
        helper(
            "key",
            "--pid",
            str(pid),
            "--window-id",
            str(window["id"]),
            "--path",
            str(run["binary_realpath"]),
            "--title",
            title,
            "--key",
            args.key,
        )
        action = {"kind": "key", "key": args.key}
    action.update({"at": time.time(), "event_count_before": event_count_before})
    action["verified"] = False
    if expectation:
        action["expected"] = expectation
        try:
            state = await_ui_state(output, pid, event_count_before, expectation, args.timeout)
            action["observed"] = {key: state[key] for key in expectation}
            action["verified"] = True
        except RunnerError:
            action["verification"] = "timeout"
            append_json(output / "actions.jsonl", action)
            raise
    append_json(output / "actions.jsonl", action)
    print(json.dumps({"submitted": True, "verified": action["verified"], "action": action}, indent=2, sort_keys=True))


def screenshot(window_id: int, destination: Path) -> None:
    command = ["screencapture", "-x", "-o", "-l", str(window_id), str(destination)]
    completed = subprocess.run(command, text=True, capture_output=True, check=False)
    if completed.returncode != 0 or not destination.is_file():
        detail = (completed.stderr or completed.stdout).strip()
        fail(f"screencapture failed: {detail or completed.returncode}")


def temporary_screenshot(output: Path, window_id: int) -> Path:
    fd, name = tempfile.mkstemp(prefix="capture-", suffix=".png", dir=output)
    os.close(fd)
    path = Path(name)
    try:
        path.unlink()
        screenshot(window_id, path)
    except BaseException:
        path.unlink(missing_ok=True)
        raise
    return path


def relative_owned_path(root: Path, path: Path) -> str:
    root = Path(os.path.realpath(root))
    candidate = Path(os.path.realpath(path))
    try:
        return str(candidate.relative_to(root))
    except ValueError:
        fail(f"owned image path is outside run output: {path}")


def record_owned_image(
    run: dict[str, Any], root: Path, path: Path, *, kind: str, case: str | None, digest: str | None = None
) -> dict[str, Any]:
    entry = {
        "path": relative_owned_path(root, path),
        "kind": kind,
        "sha256": digest or sha256(path),
    }
    if case is not None:
        entry["case"] = case
    inventory = run.setdefault(IMAGE_INVENTORY_KEY, [])
    if not isinstance(inventory, list):
        fail("run image inventory is invalid")
    inventory[:] = [item for item in inventory if not isinstance(item, dict) or item.get("path") != entry["path"]]
    inventory.append(entry)
    return entry


def case_metadata(output: Path, case: str) -> tuple[Path, dict[str, Any]]:
    case_dir = output / case
    if case_dir.is_symlink() or not case_dir.is_dir():
        fail(f"capture case does not exist: {case}")
    metadata_path = case_dir / "metadata.json"
    if metadata_path.is_symlink():
        fail(f"capture metadata is a symlink: {metadata_path}")
    try:
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read capture metadata: {error}")
    if not isinstance(metadata, dict):
        fail(f"invalid capture metadata: {metadata_path}")
    return metadata_path, metadata


def reviewed_case(output: Path, case: str) -> tuple[Path, dict[str, Any]] | None:
    case_dir = output / case
    result_path = case_dir / "result.json"
    if not result_path.is_file() or result_path.is_symlink():
        return None
    try:
        result = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read review result: {error}")
    if not isinstance(result, dict) or result.get("verdict") not in {"pass", "fail", "blocked"}:
        fail(f"invalid review result: {result_path}")
    return result_path, result


def inventory_path(root: Path, entry: dict[str, Any]) -> Path:
    raw = entry.get("path")
    if not isinstance(raw, str) or not raw or Path(raw).is_absolute() or ".." in Path(raw).parts:
        fail(f"owned image path is unsafe: {raw!r}")
    path = root / raw
    cursor = root
    for part in Path(raw).parts:
        cursor /= part
        if cursor.is_symlink():
            fail(f"owned image path is a symlink: {cursor}")
    resolved = Path(os.path.realpath(path))
    try:
        resolved.relative_to(root)
    except ValueError:
        fail(f"owned image path is outside run output: {path}")
    return path


def review(args: argparse.Namespace) -> None:
    output = absolute_output(args.output)
    case = safe_case(args.case)
    metadata_path, metadata = case_metadata(output, case)
    note = " ".join(str(args.note).replace("\x00", "").split())[:2000]
    result = {
        "case": case,
        "verdict": args.verdict,
        "note": note,
        "reviewed_at": time.time(),
        "image_deleted": bool(metadata.get("image_deleted", False)),
    }
    write_json(metadata_path.parent / "result.json", result)
    metadata["review"] = {"verdict": args.verdict, "note": note, "reviewed_at": result["reviewed_at"]}
    write_json(metadata_path, metadata)
    print(json.dumps(result, indent=2, sort_keys=True))


def cleanup(args: argparse.Namespace) -> None:
    output = absolute_output(args.output)
    root = Path(os.path.realpath(output))
    run = load_run(output)
    recorded_root = run.get("output_realpath")
    if recorded_root is not None and os.path.realpath(str(recorded_root)) != str(root):
        fail("cleanup output does not match the run output")
    inventory = run.get(IMAGE_INVENTORY_KEY)
    if not isinstance(inventory, list):
        fail("run image inventory is missing or invalid")

    deletions: list[tuple[Path, dict[str, Any], str | None]] = []
    skipped: list[str] = []
    for raw_entry in inventory:
        if not isinstance(raw_entry, dict):
            fail("run image inventory contains an invalid entry")
        path = inventory_path(root, raw_entry)
        kind = raw_entry.get("kind")
        case = raw_entry.get("case")
        if kind == "finalized":
            if not isinstance(case, str) or safe_case(case) != case:
                fail(f"owned finalized image has invalid case: {case!r}")
            if Path(raw_entry["path"]) != Path(case) / "window.png":
                fail(f"owned finalized image is not the case window: {path}")
            if reviewed_case(output, case) is None:
                skipped.append(case)
                continue
            metadata_path, metadata = case_metadata(output, case)
            if metadata.get("image") not in {"window.png", None} and not metadata.get("image_deleted"):
                fail(f"capture metadata points to an unexpected image for {case}")
            expected = metadata.get("image_sha256")
            if not isinstance(expected, str) or raw_entry.get("sha256") != expected:
                fail(f"image metadata hash does not match inventory for {case}")
            if path.exists():
                if sha256(path) != expected:
                    fail(f"image hash does not match metadata for {path}")
                deletions.append((path, raw_entry, case))
            continue
        if kind == "transient":
            name = path.name
            if not name.startswith("capture-") or path.suffix != ".png":
                fail(f"unrecognized owned temporary frame: {path}")
            if path.exists():
                expected = raw_entry.get("sha256")
                if not isinstance(expected, str) or sha256(path) != expected:
                    fail(f"temporary image hash does not match inventory for {path}")
                deletions.append((path, raw_entry, None))
            continue
        fail(f"unknown owned image kind: {kind!r}")

    for path, entry, _ in deletions:
        path.unlink()
        entry["deleted"] = True
        entry["deleted_at"] = time.time()

    touched_cases: set[str] = set()
    for _, _, case in deletions:
        if case is not None:
            touched_cases.add(case)
    for case in touched_cases:
        metadata_path, metadata = case_metadata(output, case)
        metadata.pop("image", None)
        metadata["image_deleted"] = True
        write_json(metadata_path, metadata)
        result_path = output / case / "result.json"
        result = json.loads(result_path.read_text(encoding="utf-8"))
        result.pop("image", None)
        result["image_deleted"] = True
        write_json(result_path, result)
    write_json(output / "run.json", run)
    print(json.dumps({"deleted": [str(path.relative_to(root)) for path, _, _ in deletions], "skipped_unreviewed": sorted(set(skipped))}, indent=2, sort_keys=True))


def _capture(args: argparse.Namespace, owned_tempfiles: set[Path], capture_state: dict[str, Any]) -> None:
    require_desktop(capture=True)
    output = absolute_output(args.output)
    case = safe_case(args.case)
    run = load_run(output)
    process_identity(run)
    window = update_window(output, run, persist=False)
    case_dir = output / case
    image = case_dir / "window.png"
    metadata_path = case_dir / "metadata.json"
    telemetry_path = case_dir / "telemetry.json"
    if image.exists():
        fail(f"refusing to overwrite existing capture: {image}")
    if metadata_path.exists() or telemetry_path.exists():
        fail(f"refusing to overwrite existing capture metadata: {case_dir}")

    pending = "pending" in case.lower()
    action_log = output / "actions.jsonl"
    actions: list[dict[str, Any]] = []
    if action_log.is_file():
        for line in action_log.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                actions.append(value)
    baseline = actions[-1].get("event_count_before") if actions else None
    deadline = time.monotonic() + args.timeout
    transient = False
    stable_path: Path | None = None
    owned_temp_hashes: dict[Path, str] = {}
    previous_hash: str | None = None
    stable_count = 0
    checkpoint = False
    settled = False
    while time.monotonic() < deadline:
        events = read_events(output)
        last = events[-1] if events else None
        if baseline is None:
            checkpoint = bool(last and last.get("event") == "ready")
        elif len(events) > int(baseline):
            checkpoint = bool(last and last.get("event") in TERMINAL_EVENTS)
            if pending and last and last.get("event") == "test-start":
                checkpoint = True
                transient = True
        if checkpoint and transient:
            if stable_path is not None:
                stable_path.unlink(missing_ok=True)
            stable_path = temporary_screenshot(output, int(window["id"]))
            owned_tempfiles.add(stable_path)
            owned_temp_hashes[stable_path] = sha256(stable_path)
            settled = True
            break
        current = temporary_screenshot(output, int(window["id"]))
        owned_tempfiles.add(current)
        current_hash = sha256(current)
        owned_temp_hashes[current] = current_hash
        if current_hash == previous_hash:
            stable_count += 1
        else:
            stable_count = 1
        previous_hash = current_hash
        if stable_count >= 2:
            if stable_path is not None:
                stable_path.unlink(missing_ok=True)
            stable_path = current
            settled = True
            break
        if stable_path is not None:
            stable_path.unlink(missing_ok=True)
        stable_path = current
        time.sleep(0.2)
    if not settled or stable_path is None:
        if stable_path is not None:
            stable_path.unlink(missing_ok=True)
        fail("screen did not reach an event checkpoint or stable pair before timeout")

    case_dir.mkdir(parents=True, exist_ok=False)
    os.replace(stable_path, image)
    capture_state["image"] = image
    capture_state["metadata"] = metadata_path
    capture_state["telemetry"] = telemetry_path
    events = read_events(output)
    last_event = events[-1] if events else None
    image_hash = sha256(image)
    finalized_entry = record_owned_image(run, output, image, kind="finalized", case=case, digest=image_hash)
    finalized_entry["finalized"] = True
    for transient_path, transient_hash in owned_temp_hashes.items():
        if transient_path != stable_path:
            record_owned_image(run, output, transient_path, kind="transient", case=case, digest=transient_hash)
    captured_at = time.time()
    metadata = {
        "case": case,
        "image": "window.png",
        "image_sha256": image_hash,
        "finalized": True,
        "captured_at": captured_at,
        "transient": transient,
        "actions": actions,
        "event": last_event.get("event") if last_event else None,
        "outcome": last_event.get("outcome") if last_event else None,
        "report": None,
        "binary": run["binary_realpath"],
        "binary_sha256": run["binary_sha256"],
        "pid": run["pid"],
        "window": window,
    }
    telemetry = {
        "captured_at": captured_at,
        "image_sha256": image_hash,
        "events": events,
        "last_event": last_event,
        "window": window,
        "stdout_log": run["stdout_log"],
        "hang_incidents": read_hang_incidents(output),
    }
    write_json(metadata_path, metadata)
    write_json(telemetry_path, telemetry)
    write_json(output / "run.json", run)
    capture_state["finalized"] = True
    print(json.dumps(metadata, indent=2, sort_keys=True))


def capture(args: argparse.Namespace) -> None:
    owned_tempfiles: set[Path] = set()
    capture_state: dict[str, Any] = {"finalized": False}
    try:
        _capture(args, owned_tempfiles, capture_state)
    finally:
        if not capture_state.get("finalized"):
            for key in ("image", "metadata", "telemetry"):
                path = capture_state.get(key)
                if isinstance(path, Path) and path.is_file() and not path.is_symlink():
                    path.unlink()
        for path in owned_tempfiles:
            try:
                if path.is_symlink() or not path.is_file():
                    continue
                path.unlink()
            except FileNotFoundError:
                pass


def stop(args: argparse.Namespace) -> None:
    output = absolute_output(args.output)
    run = load_run(output)
    pid, _ = process_identity(run)
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        try:
            info = helper("process-info", "--pid", str(pid))
        except RunnerError:
            info = {"alive": False}
        if not info.get("alive"):
            break
        time.sleep(0.05)
    else:
        os.kill(pid, signal.SIGKILL)
    run["status"] = "stopped"
    run["stopped_at"] = time.time()
    write_json(output / "run.json", run)
    print(json.dumps({"ok": True, "pid": pid, "status": "stopped"}, indent=2, sort_keys=True))


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description="Native macOS capture runner for TableX GPUI scenarios")
    commands = root.add_subparsers(dest="command", required=True)

    launch_parser = commands.add_parser("launch", help="launch an approved UI scenario")
    launch_parser.add_argument("--binary", required=True, help="absolute path to target/debug/tablex-gpui")
    launch_parser.add_argument("--output", required=True, help="absolute capture output directory")
    launch_parser.add_argument("--scenario", choices=SCENARIOS, default=DEFAULT_SCENARIO)
    launch_parser.add_argument("--fixture", choices=FIXTURES, default=DEFAULT_FIXTURE)
    launch_parser.add_argument("--theme", choices=THEMES, help="force the theme for this run (sets TABLEX_THEME); default: the scenario's saved/default theme")
    launch_parser.add_argument("--timeout", type=float, default=20.0)
    launch_parser.add_argument("--window-timeout", type=float, default=5.0)
    launch_parser.set_defaults(handler=launch)

    act_parser = commands.add_parser("act", help="send one native interaction")
    act_parser.add_argument("--output", required=True)
    act_parser.add_argument("--expect", action="append", default=[], metavar="FIELD=VALUE", help="require a fresh matching non-sensitive UI state after input; repeat for multiple fields")
    act_parser.add_argument("--timeout", type=float, default=3.0)
    group = act_parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--click", nargs=2, metavar=("X", "Y"), type=float)
    group.add_argument("--click-id", choices=tuple(CLICK_TARGET_STEPS))
    group.add_argument("--double-click", nargs=2, metavar=("X", "Y"), type=float)
    group.add_argument("--right-click", nargs=2, metavar=("X", "Y"), type=float)
    act_parser.add_argument("--click-modifier", choices=("shift", "cmd"), help="hold one supported modifier during --click or --click-id")
    group.add_argument("--drag", nargs=4, metavar=("X1", "Y1", "X2", "Y2"), type=float)
    group.add_argument("--move", nargs=2, action="append", metavar=("X", "Y"), type=float, help="append a button-free mouse move to a paced trace")
    act_parser.add_argument("--move-hz", type=int, default=60, help="mouse-move trace cadence (1-120 Hz)")
    group.add_argument("--text")
    group.add_argument("--key", choices=("escape", "tab", "enter", "cmd-a", "cmd-f", "cmd-k", "cmd-shift-g", "backspace", "left", "right", "up", "down", "shift-left", "shift-right", "shift-up", "shift-down", "home", "end", "page-up", "page-down"))
    act_parser.set_defaults(handler=act)

    capture_parser = commands.add_parser("capture", help="capture one owned window into a new case directory")
    capture_parser.add_argument("--output", required=True)
    capture_parser.add_argument("--case", required=True)
    capture_parser.add_argument("--timeout", type=float, default=5.0)
    capture_parser.set_defaults(handler=capture)

    stop_parser = commands.add_parser("stop", help="stop only the process recorded in run.json")
    stop_parser.add_argument("--output", required=True)
    stop_parser.add_argument("--timeout", type=float, default=3.0)
    stop_parser.set_defaults(handler=stop)

    review_parser = commands.add_parser("review", help="record a manual visual review without deleting images")
    review_parser.add_argument("--output", required=True)
    review_parser.add_argument("--case", required=True)
    review_parser.add_argument("--verdict", choices=("pass", "fail", "blocked"), required=True)
    review_parser.add_argument("--note", default="")
    review_parser.set_defaults(handler=review)

    cleanup_parser = commands.add_parser("cleanup", help="delete only reviewed images recorded in the run manifest")
    cleanup_parser.add_argument("--output", required=True)
    cleanup_parser.set_defaults(handler=cleanup)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        args.handler(args)
    except RunnerError as error:
        print(f"capture-gpui: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
