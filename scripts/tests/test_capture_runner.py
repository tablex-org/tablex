import argparse
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("capture_runner", Path(__file__).parents[1] / "capture-gpui.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class CaptureRunnerTests(unittest.TestCase):
    def _reviewed_run(self, output: Path, image: bytes = b"png"):
        case_dir = output / "approved"
        case_dir.mkdir()
        image_path = case_dir / "window.png"
        image_path.write_bytes(image)
        digest = runner.sha256(image_path)
        runner.write_json(
            output / "run.json",
            {
                "output_realpath": str(output.resolve()),
                "owned_images": [
                    {
                        "path": "approved/window.png",
                        "kind": "finalized",
                        "case": "approved",
                        "sha256": digest,
                    }
                ],
            },
        )
        runner.write_json(
            case_dir / "metadata.json",
            {"case": "approved", "image": "window.png", "image_sha256": digest, "report": None},
        )
        runner.write_json(
            case_dir / "result.json",
            {"case": "approved", "verdict": "pass", "note": "read", "image_deleted": False},
        )
        return case_dir, image_path

    def test_expected_state_requires_a_fresh_observation_from_owned_pid(self):
        events = [{"event": "ui-state", "pid": 42, "state": {"category": "Analytics"}}]
        with patch.object(runner, "read_events", return_value=events), patch.object(runner.time, "monotonic", side_effect=[0, 0, 2]), patch.object(runner.time, "sleep"):
            with self.assertRaises(runner.RunnerError):
                runner.await_ui_state(Path("unused"), 42, 1, {"category": "Analytics"}, 1)
        events.append({"event": "ui-state", "pid": 42, "state": {"category": "Relational"}})
        with patch.object(runner, "read_events", return_value=events):
            self.assertEqual(runner.await_ui_state(Path("unused"), 42, 1, {"category": "Relational"}, 1), events[-1]["state"])

    def test_other_process_observation_is_rejected(self):
        events = [{"event": "ui-state", "pid": 43, "state": {"category": "Analytics"}}]
        with patch.object(runner, "read_events", return_value=events), patch.object(runner.time, "monotonic", side_effect=[0, 0, 2]), patch.object(runner.time, "sleep"):
            with self.assertRaises(runner.RunnerError):
                runner.await_ui_state(Path("unused"), 42, 0, {"category": "Analytics"}, 1)

    def test_observation_fields_never_accept_credentials(self):
        with self.assertRaises(runner.RunnerError):
            runner.expected_state(["password=SECRET"])
        self.assertEqual(runner.expected_state(["step=config", "search_length=5", "next_enabled=false"]), {"step": "config", "search_length": 5, "next_enabled": False})

    def test_layout_and_validation_observations_are_numeric_or_boolean(self):
        self.assertEqual(runner.expected_state(["column_widths=[160,260]", "inline_error=true", "deleted_count=1"]), {"column_widths": [160, 260], "inline_error": True, "deleted_count": 1})
        self.assertEqual(runner.expected_state(["column_widths=[]"]), {"column_widths": []})
        for value in ('["secret"]', '[true]', '[NaN]', '[-1]', '[99999]'):
            with self.assertRaises(runner.RunnerError):
                runner.expected_state([f"column_widths={value}"])

    def test_populated_observations_are_whitelisted_without_free_form_text(self):
        self.assertEqual(
            runner.expected_state(["selected_node=none", "active_tab=orders", "page_rows=50", "total_rows=240", "page_offset=0", "first_row_id=1", "pending_count=1"]),
            {"selected_node": None, "active_tab": "orders", "page_rows": 50, "total_rows": 240, "page_offset": 0, "first_row_id": 1, "pending_count": 1},
        )
        with self.assertRaises(runner.RunnerError):
            runner.expected_state(["active_tab=secret-tab"])

    def test_typing_is_redacted_and_not_claimed_verified_without_expectation(self):
        with tempfile.TemporaryDirectory() as temp:
            args = argparse.Namespace(output=temp, click=None, text="mongodb://user:SECRET@example.test/db", key=None, expect=[], timeout=1)
            run = {"binary_realpath": "/fixture/binary"}
            out = io.StringIO()
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity", return_value=(42, {})), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "helper"), contextlib.redirect_stdout(out):
                runner.act(args)
            log = (Path(temp) / "actions.jsonl").read_text()
            self.assertNotIn("SECRET", log + out.getvalue())
            self.assertNotIn("mongodb://", log + out.getvalue())
            self.assertFalse(json.loads(out.getvalue())["verified"])

    def test_mongo_grid_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("mongo-grid"),
            "TableX UI Scenario: mongo-grid",
        )
        self.assertIn("mongo-grid", runner.SCENARIOS)

    def test_table_diagram_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("table-diagram"),
            "TableX UI Scenario: table-diagram",
        )
        self.assertIn("table-diagram", runner.SCENARIOS)
        self.assertIn("Track", runner.KNOWN_ACTIVE_TABS)

    def test_database_diagram_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("database-diagram"),
            "TableX UI Scenario: database-diagram",
        )
        self.assertIn("database-diagram", runner.SCENARIOS)
        self.assertIn("main diagram", runner.KNOWN_ACTIVE_TABS)

    def test_table_properties_edit_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("table-properties-edit"),
            "TableX UI Scenario: table-properties-edit",
        )
        self.assertIn("table-properties-edit", runner.SCENARIOS)
        self.assertIn("orders", runner.KNOWN_ACTIVE_TABS)
        self.assertIn(
            '"TableX UI Scenario: table-properties-edit"',
            Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text(),
        )

    def test_grouped_tabs_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("grouped-tabs"),
            "TableX UI Scenario: grouped-tabs",
        )
        self.assertIn("grouped-tabs", runner.SCENARIOS)
        self.assertIn("grouped-tabs-lone", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: grouped-tabs"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_settings_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("settings"),
            "TableX UI Scenario: settings",
        )
        self.assertIn("settings", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: settings"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_settings_shortcuts_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("settings-shortcuts"),
            "TableX UI Scenario: settings-shortcuts",
        )
        self.assertIn("settings-shortcuts", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: settings-shortcuts"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_connection_overrides_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("connection-overrides"),
            "TableX UI Scenario: connection-overrides",
        )
        self.assertIn("connection-overrides", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: connection-overrides"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_connection_overview_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("connection-overview"),
            "TableX UI Scenario: connection-overview",
        )
        self.assertIn("connection-overview", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: connection-overview"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_import_selection_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("import-selection"),
            "TableX UI Scenario: import-selection",
        )
        self.assertIn("import-selection", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: import-selection"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_redis_is_an_approved_scenario(self):
        self.assertEqual(
            runner.scenario_title("redis"),
            "TableX UI Scenario: redis",
        )
        self.assertIn("redis", runner.SCENARIOS)
        self.assertIn('"TableX UI Scenario: redis"', Path(runner.__file__).with_name("gpui-capture-helper.swift").read_text())

    def test_launch_theme_flag_forces_the_scenario_theme_through_the_environment(self):
        args = runner.parser().parse_args(["launch", "--binary", "/x", "--output", "/y", "--theme", "light"])
        self.assertEqual(args.theme, "light")
        self.assertEqual(runner.launch_env(args.theme)["TABLEX_THEME"], "light")
        with patch.dict(runner.os.environ, {}, clear=False):
            runner.os.environ.pop("TABLEX_THEME", None)
            self.assertNotIn("TABLEX_THEME", runner.launch_env(None))
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            runner.parser().parse_args(["launch", "--binary", "/x", "--output", "/y", "--theme", "sepia"])

    def test_launch_refuses_symlink_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            target = root / "real-output"
            target.mkdir()
            link = root / "linked-output"
            link.symlink_to(target, target_is_directory=True)
            args = argparse.Namespace(output=str(link), binary="/fixture/binary", timeout=1, window_timeout=1)
            with patch.object(runner, "require_desktop"), patch.object(runner, "binary_path") as binary_path, patch.object(runner, "helper") as helper, patch.object(runner.subprocess, "Popen") as popen:
                with self.assertRaisesRegex(runner.RunnerError, "symlink"):
                    runner.launch(args)
            binary_path.assert_not_called()
            helper.assert_not_called()
            popen.assert_not_called()

    def test_act_supports_native_right_click_and_drag_without_helper_verification(self):
        with tempfile.TemporaryDirectory() as temp:
            args = argparse.Namespace(
                output=temp,
                click=None,
                double_click=None,
                right_click=[100, 200],
                drag=None,
                text=None,
                key=None,
                expect=[],
                timeout=1,
            )
            run = {"binary_realpath": "/fixture/binary"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity", return_value=(42, {})), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "helper") as helper, contextlib.redirect_stdout(io.StringIO()):
                helper.return_value = {"event_count": 3, "sent_uptime_ns": [1, 2, 3]}
                runner.act(args)
            self.assertEqual(helper.call_args.args[0], "right-click")
            self.assertFalse(json.loads((Path(temp) / "actions.jsonl").read_text())["verified"])

            args.right_click = None
            args.drag = [10, 20, 300, 400]
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity", return_value=(42, {})), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "helper") as helper, contextlib.redirect_stdout(io.StringIO()):
                runner.act(args)
            self.assertEqual(helper.call_args.args[0], "drag")

    def test_act_batches_pure_mouse_moves_in_one_helper_call(self):
        with tempfile.TemporaryDirectory() as temp:
            args = argparse.Namespace(
                output=temp,
                click=None,
                double_click=None,
                right_click=None,
                drag=None,
                move=[[100, 200], [120, 220], [140, 240]],
                move_hz=120,
                text=None,
                key=None,
                expect=[],
                timeout=1,
            )
            run = {"binary_realpath": "/fixture/binary"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity", return_value=(42, {})), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "helper") as helper, contextlib.redirect_stdout(io.StringIO()):
                helper.return_value = {"event_count": 3, "sent_uptime_ns": [1, 2, 3]}
                runner.act(args)
            self.assertEqual(helper.call_args.args[0], "mouse-move-trace")
            self.assertEqual(helper.call_args.kwargs["stdin"], json.dumps({"points": [[100.0, 200.0], [120.0, 220.0], [140.0, 240.0]], "hz": 120}))
            action = json.loads((Path(temp) / "actions.jsonl").read_text())
            self.assertEqual(action["kind"], "mouse-move-trace")
            self.assertEqual(action["points"], 3)

    def test_parser_accepts_bounded_drag_and_navigation_keys(self):
        parsed = runner.parser().parse_args(["act", "--output", "/tmp/run", "--drag", "1", "2", "3", "4"])
        self.assertEqual(parsed.drag, [1.0, 2.0, 3.0, 4.0])
        parsed = runner.parser().parse_args(["act", "--output", "/tmp/run", "--move", "1", "2", "--move", "3", "4"])
        self.assertEqual(parsed.move, [[1.0, 2.0], [3.0, 4.0]])
        for key in (
            "left", "right", "up", "down", "home", "end", "page-up", "page-down",
            "shift-left", "shift-right", "shift-up", "shift-down", "cmd-shift-g",
        ):
            self.assertEqual(runner.parser().parse_args(["act", "--output", "/tmp/run", "--key", key]).key, key)
        parsed = runner.parser().parse_args(["act", "--output", "/tmp/run", "--click", "1", "2", "--click-modifier", "shift"])
        self.assertEqual(parsed.click_modifier, "shift")

    def test_click_modifier_reaches_owned_native_click(self):
        with tempfile.TemporaryDirectory() as temp:
            args = argparse.Namespace(
                output=temp, click=[100, 200], click_modifier="shift", double_click=None,
                right_click=None, drag=None, move=None, text=None, key=None, expect=[], timeout=1,
            )
            run = {"binary_realpath": "/fixture/binary"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity", return_value=(42, {})), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "helper") as helper, contextlib.redirect_stdout(io.StringIO()):
                runner.act(args)
            self.assertEqual(helper.call_args.args[:2], ("click", "--pid"))
            self.assertIn("--modifier", helper.call_args.args)
            self.assertEqual(helper.call_args.args[helper.call_args.args.index("--modifier") + 1], "shift")
            action = json.loads((Path(temp) / "actions.jsonl").read_text())
            self.assertEqual(action["modifier"], "shift")

    def test_click_modifier_is_limited_to_supported_single_clicks(self):
        args = argparse.Namespace(
            output="/unused", click=None, click_id=None, click_modifier="cmd", double_click=None,
            right_click=[1, 2], drag=None, move=None, text=None, key=None, expect=[], timeout=1,
        )
        with patch.object(runner, "require_desktop") as require_desktop, patch.object(runner, "load_run") as load:
            with self.assertRaisesRegex(runner.RunnerError, "modifier"):
                runner.act(args)
        require_desktop.assert_not_called()
        load.assert_not_called()

    def test_parser_accepts_semantic_click_ids(self):
        parsed = runner.parser().parse_args(
            ["act", "--output", "/tmp/run", "--click-id", "connection-picker-next"]
        )
        self.assertEqual(parsed.click_id, "connection-picker-next")

    def test_semantic_click_resolves_latest_rendered_bounds(self):
        events = [
            {
                "event": "ui-bounds",
                "pid": 42,
                "target": "connection-picker-next",
                "step": "picker",
                "dialog_id": 1,
                "window_id": 7,
                "enabled": True,
                "visible": True,
                "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
                "window_width": 400.0,
                "window_height": 500.0,
            }
        ]
        result = runner.resolve_click_id(
            events,
            pid=42,
            target="connection-picker-next",
            window={
                "id": 101,
                "ui_window_id": 7,
                "bounds": {"width": 400.0, "height": 500.0},
                "content_bounds": {"width": 400.0, "height": 500.0},
            },
            current_state={"step": "picker", "dialog_id": 1},
        )
        self.assertEqual(result["x"], 140.0)
        self.assertEqual(result["y"], 220.0)
        self.assertEqual(result["bounds"]["width"], 80.0)

    def test_semantic_click_rejects_stale_or_unusable_bounds(self):
        base = {
            "event": "ui-bounds",
            "pid": 42,
            "target": "connection-picker-next",
            "step": "picker",
            "dialog_id": 1,
            "window_id": 7,
            "enabled": True,
            "visible": True,
            "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
            "window_width": 400.0,
            "window_height": 500.0,
        }
        window = {
            "id": 101,
            "ui_window_id": 7,
            "bounds": {"width": 400.0, "height": 500.0},
            "content_bounds": {"width": 400.0, "height": 500.0},
        }
        for mutation, message in (
            ({"pid": 41}, "owned pid"),
            ({"step": "config"}, "step"),
            ({"enabled": False}, "disabled"),
            ({"visible": False}, "visible"),
            ({"bounds": {"x": 100.0, "y": 200.0, "width": 0.0, "height": 40.0}}, "bounds"),
            ({"bounds": {"x": 380.0, "y": 200.0, "width": 80.0, "height": 40.0}}, "window"),
        ):
            event = {**base, **mutation}
            with self.subTest(message=message):
                with self.assertRaisesRegex(runner.RunnerError, message):
                    runner.resolve_click_id(
                        [event],
                        pid=42,
                        target="connection-picker-next",
                        window=window,
                        current_state={"step": "picker", "dialog_id": 1},
                    )

    def test_semantic_click_rejects_stale_next_after_config(self):
        event = {
            "event": "ui-bounds",
            "pid": 42,
            "target": "connection-picker-next",
            "step": "picker",
            "dialog_id": 1,
            "window_id": 7,
            "enabled": True,
            "visible": True,
            "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
            "window_width": 400.0,
            "window_height": 500.0,
        }
        with self.assertRaisesRegex(runner.RunnerError, "current step"):
            runner.resolve_click_id(
                [event],
                pid=42,
                target="connection-picker-next",
                window={
                    "id": 101,
                    "ui_window_id": 7,
                    "bounds": {"width": 400.0, "height": 500.0},
                    "content_bounds": {"width": 400.0, "height": 500.0},
                },
                current_state={"step": "config", "dialog_id": 1},
            )

    def test_semantic_click_rejects_stale_back_after_picker(self):
        event = {
            "event": "ui-bounds",
            "pid": 42,
            "target": "connection-config-type-control",
            "step": "config",
            "dialog_id": 1,
            "window_id": 7,
            "enabled": True,
            "visible": True,
            "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
            "window_width": 400.0,
            "window_height": 500.0,
        }
        with self.assertRaisesRegex(runner.RunnerError, "current step"):
            runner.resolve_click_id(
                [event],
                pid=42,
                target="connection-config-type-control",
                window={
                    "id": 101,
                    "ui_window_id": 7,
                    "bounds": {"width": 400.0, "height": 500.0},
                    "content_bounds": {"width": 400.0, "height": 500.0},
                },
                current_state={"step": "picker", "dialog_id": 1},
            )

    def test_semantic_click_rejects_old_window_geometry_even_when_target_fits(self):
        event = {
            "event": "ui-bounds",
            "pid": 42,
            "target": "connection-picker-next",
            "step": "picker",
            "dialog_id": 1,
            "window_id": 7,
            "enabled": True,
            "visible": True,
            "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
            "window_width": 400.0,
            "window_height": 500.0,
        }
        with self.assertRaisesRegex(runner.RunnerError, "current window geometry"):
            runner.resolve_click_id(
                [event],
                pid=42,
                target="connection-picker-next",
                window={
                    "id": 101,
                    "ui_window_id": 7,
                    "bounds": {"width": 500.0, "height": 600.0},
                    "content_bounds": {"width": 500.0, "height": 600.0},
                },
                current_state={"step": "picker", "dialog_id": 1},
            )

    def test_semantic_click_rejects_bounds_from_dismissed_dialog(self):
        event = {
            "event": "ui-bounds",
            "pid": 42,
            "target": "connection-picker-next",
            "step": "picker",
            "dialog_id": 1,
            "window_id": 7,
            "enabled": True,
            "visible": True,
            "bounds": {"x": 100.0, "y": 200.0, "width": 80.0, "height": 40.0},
            "window_width": 400.0,
            "window_height": 500.0,
        }
        with self.assertRaisesRegex(runner.RunnerError, "current dialog"):
            runner.resolve_click_id(
                [event],
                pid=42,
                target="connection-picker-next",
                window={
                    "id": 101,
                    "ui_window_id": 7,
                    "bounds": {"width": 400.0, "height": 500.0},
                    "content_bounds": {"width": 400.0, "height": 500.0},
                },
                current_state={"step": "picker", "dialog_id": 2},
            )

    def test_latest_dialog_state_preserves_authoritative_close_event(self):
        events = [
            {
                "event": "ui-state",
                "phase": "render",
                "pid": 42,
                "dialog_id": 1,
                "window_id": 7,
                "state": {"step": "picker"},
            },
            {
                "event": "ui-state",
                "phase": "closed",
                "pid": 42,
                "dialog_id": 1,
                "window_id": 7,
                "closed": True,
            },
            {
                "event": "ui-state",
                "phase": "render",
                "pid": 42,
                "state": {"scenario": "new-connection"},
            },
        ]
        state = runner.latest_dialog_state(events, 42)
        self.assertTrue(state["closed"])
        self.assertEqual(state["dialog_id"], 1)

    def test_never_stable_capture_times_out_instead_of_saving_last_frame(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            transient = output / "unstable.png"
            transient.write_bytes(b"not-a-stable-image")
            args = argparse.Namespace(output=temp, case="unstable", timeout=1)
            run = {"binary_realpath": "/fixture/binary", "binary_sha256": "fixture", "pid": 42, "stdout_log": "fixture.log"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity"), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[]), patch.object(runner, "temporary_screenshot", return_value=transient), patch.object(runner.time, "monotonic", side_effect=[0, 0, 2]), patch.object(runner.time, "sleep"), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(runner.RunnerError):
                    runner.capture(args)
            self.assertFalse((output / "unstable" / "window.png").exists())

    def test_locked_desktop_fails_before_input_is_sent(self):
        args = argparse.Namespace(output="/unused", click=[100, 200], text=None, key=None, expect=[], timeout=1)
        with patch.object(runner, "helper", return_value={"desktop_available": True, "locked": True}) as helper, patch.object(runner, "load_run") as load:
            with self.assertRaisesRegex(runner.RunnerError, "locked"):
                runner.act(args)
            helper.assert_called_once_with("environment")
            load.assert_not_called()

    def test_launch_reaps_child_if_initial_helper_or_state_write_fails(self):
        for failure in ("helper", "state-write"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temp:
                args = argparse.Namespace(output=temp, binary="/fixture/binary", timeout=1, window_timeout=1)
                with patch.object(runner, "require_desktop"), patch.object(runner, "binary_path", return_value=Path("/fixture/binary")), patch.object(runner, "sha256", return_value="hash"), patch.object(runner.subprocess, "Popen") as popen, patch.object(runner, "helper") as helper, patch.object(runner, "write_json") as write:
                    child = popen.return_value
                    child.pid = 42
                    child.poll.return_value = None
                    if failure == "helper":
                        helper.side_effect = runner.RunnerError("helper failed")
                    else:
                        helper.return_value = {"alive": True, "path": "/fixture/binary"}
                        write.side_effect = OSError("state write failed")
                    with self.assertRaises((runner.RunnerError, OSError)):
                        runner.launch(args)
                    child.terminate.assert_called_once()
                    child.wait.assert_called_once()

    def test_launch_refuses_nonempty_output_before_writes_or_process_start(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            old_run = output / "run.json"
            old_image = output / "old.png"
            old_run.write_text('{"status":"stopped","owned_images":["old.png"]}\n')
            old_image.write_bytes(b"keep")
            before = {path: path.read_bytes() for path in (old_run, old_image)}
            args = argparse.Namespace(output=temp, binary="/fixture/binary", timeout=1, window_timeout=1)
            with patch.object(runner, "require_desktop"), patch.object(runner, "binary_path") as binary_path, patch.object(runner, "sha256", return_value="hash"), patch.object(runner, "helper") as helper, patch.object(runner.subprocess, "Popen") as popen:
                with self.assertRaisesRegex(runner.RunnerError, "fresh"):
                    runner.launch(args)
            binary_path.assert_not_called()
            helper.assert_not_called()
            popen.assert_not_called()
            self.assertEqual(before, {path: path.read_bytes() for path in (old_run, old_image)})

    def test_capture_stop_and_rerun_refuse_stale_output_without_mutation(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            old_run = output / "run.json"
            old_image = output / "old.png"
            old_run.write_text('{"status":"stopped","owned_images":["old.png"]}\n')
            old_image.write_bytes(b"keep")
            before = {path: path.read_bytes() for path in (old_run, old_image)}
            stale = {"pid": 42, "binary_realpath": "/fixture/binary", "binary_sha256": "old"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=stale), patch.object(runner, "process_identity", side_effect=runner.RunnerError("stale process")), patch.object(runner, "update_window"):
                with self.assertRaises(runner.RunnerError):
                    runner.capture(argparse.Namespace(output=temp, case="new", timeout=1))
                with self.assertRaises(runner.RunnerError):
                    runner.stop(argparse.Namespace(output=temp, timeout=1))
            with patch.object(runner, "require_desktop"), patch.object(runner.subprocess, "Popen") as popen:
                with self.assertRaises(runner.RunnerError):
                    runner.launch(argparse.Namespace(output=temp, binary="/fixture/binary", timeout=1, window_timeout=1))
            popen.assert_not_called()
            self.assertEqual(before, {path: path.read_bytes() for path in (old_run, old_image)})

    def test_capture_write_failures_remove_promoted_image_and_partial_metadata(self):
        for failed_name in ("metadata.json", "telemetry.json", "run.json"):
            with self.subTest(failed_name=failed_name), tempfile.TemporaryDirectory() as temp:
                output = Path(temp)
                user_image = output / "user.png"
                user_image.write_bytes(b"keep")
                frames = []
                for index in (1, 2):
                    frame = output / f"frame-{index}.png"
                    frame.write_bytes(b"stable")
                    frames.append(frame)
                run_path = output / "run.json"
                run_path.write_text('{"status":"running","owned_images":[]}\n')
                original_write_json = runner.write_json

                def write_json(path, value):
                    if Path(path).name == failed_name:
                        raise OSError(f"injected {failed_name} failure")
                    return original_write_json(path, value)

                args = argparse.Namespace(output=temp, case="failed", timeout=1)
                run = {"binary_realpath": "/fixture/binary", "binary_sha256": "fixture", "pid": 42, "stdout_log": "fixture.log"}
                with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity"), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[{"event": "ready", "pid": 42}]), patch.object(runner, "temporary_screenshot", side_effect=frames), patch.object(runner, "write_json", side_effect=write_json), patch.object(runner.time, "monotonic", side_effect=[0, 0, 0]), patch.object(runner.time, "sleep"):
                    with self.assertRaises(OSError):
                        runner.capture(args)
                self.assertFalse((output / "failed" / "window.png").exists())
                self.assertFalse((output / "failed" / "metadata.json").exists())
                self.assertFalse((output / "failed" / "telemetry.json").exists())
                self.assertTrue(user_image.exists())
                self.assertTrue(all(not frame.exists() for frame in frames))

    def test_stdout_failure_does_not_delete_finalized_image(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            frames = []
            for index in (1, 2):
                frame = output / f"frame-{index}.png"
                frame.write_bytes(b"stable")
                frames.append(frame)
            args = argparse.Namespace(output=temp, case="review-me", timeout=1)
            run = {"binary_realpath": "/fixture/binary", "binary_sha256": "fixture", "pid": 42, "stdout_log": "fixture.log"}
            with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity"), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "read_events", return_value=[{"event": "ready", "pid": 42}]), patch.object(runner, "temporary_screenshot", side_effect=frames), patch.object(runner.time, "monotonic", side_effect=[0, 0, 0]), patch.object(runner.time, "sleep"), patch("builtins.print", side_effect=BrokenPipeError):
                with self.assertRaises(BrokenPipeError):
                    runner.capture(args)
            self.assertTrue((output / "review-me" / "window.png").exists())
            self.assertTrue((output / "review-me" / "metadata.json").exists())
            self.assertTrue((output / "review-me" / "telemetry.json").exists())

    def _capture_with_stdout_log(self, output: Path, lines: list[str]):
        frames = []
        for index in (1, 2):
            frame = output / f"frame-{index}.png"
            frame.write_bytes(b"stable")
            frames.append(frame)
        (output / "stdout.log").write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
        args = argparse.Namespace(output=str(output), case="hangs", timeout=1)
        run = {"binary_realpath": "/fixture/binary", "binary_sha256": "fixture", "pid": 42, "stdout_log": "stdout.log"}
        with patch.object(runner, "require_desktop"), patch.object(runner, "load_run", return_value=run), patch.object(runner, "process_identity"), patch.object(runner, "update_window", return_value={"id": 9}), patch.object(runner, "temporary_screenshot", side_effect=frames), patch.object(runner.time, "sleep"), contextlib.redirect_stdout(io.StringIO()):
            runner.capture(args)
        return json.loads((output / "hangs" / "telemetry.json").read_text(encoding="utf-8"))

    def test_telemetry_carries_perf_diagnostics_hang_incidents_without_disturbing_events(self):
        hang = {
            "event": "hang",
            "pid": 42,
            "poll": "interval",
            "threshold_ms": 50.0,
            "frame_budget_ms": 16.6,
            "incident": {"trigger": "threshold", "stall_ms": 90.0, "sealed_by": "present", "contributors": [{"kind": "draw", "duration_ms": 90.0}]},
        }
        with tempfile.TemporaryDirectory() as temp:
            telemetry = self._capture_with_stdout_log(
                Path(temp),
                [
                    json.dumps({"event": "ready", "pid": 42}),
                    json.dumps(hang),
                    "not json",
                    json.dumps({"event": "hang", "pid": 42, "incident": "malformed"}),
                ],
            )
        self.assertEqual(telemetry["hang_incidents"], [hang])
        # Hang lines must not become the checkpoint event or appear among UI events.
        self.assertEqual([event["event"] for event in telemetry["events"]], ["ready"])
        self.assertEqual(telemetry["last_event"]["event"], "ready")

    def test_telemetry_hang_incidents_default_to_an_empty_list(self):
        with tempfile.TemporaryDirectory() as temp:
            telemetry = self._capture_with_stdout_log(Path(temp), [json.dumps({"event": "ready", "pid": 42})])
        self.assertEqual(telemetry["hang_incidents"], [])

    def test_cleanup_deletes_only_reviewed_owned_image_and_marks_result(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            case_dir, image = self._reviewed_run(output)
            user_image = output / "user.png"
            user_image.write_bytes(b"keep")

            runner.cleanup(argparse.Namespace(output=temp))

            self.assertFalse(image.exists())
            self.assertTrue(user_image.exists())
            metadata = json.loads((case_dir / "metadata.json").read_text())
            self.assertNotIn("image", metadata)
            self.assertTrue(metadata["image_deleted"])
            result = json.loads((case_dir / "result.json").read_text())
            self.assertTrue(result["image_deleted"])

    def test_cleanup_explicitly_skips_unreviewed_case(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            case_dir = output / "unreviewed"
            case_dir.mkdir()
            image = case_dir / "window.png"
            image.write_bytes(b"png")
            runner.write_json(output / "run.json", {"owned_images": [{"path": "unreviewed/window.png", "kind": "finalized", "case": "unreviewed", "sha256": runner.sha256(image)}]})
            runner.write_json(case_dir / "metadata.json", {"case": "unreviewed", "image": "window.png", "image_sha256": runner.sha256(image)})

            output_text = io.StringIO()
            with contextlib.redirect_stdout(output_text):
                runner.cleanup(argparse.Namespace(output=temp))

            self.assertTrue(image.exists())
            self.assertIn("unreviewed", output_text.getvalue())

    def test_cleanup_refuses_tampered_image_before_deleting(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            _, image = self._reviewed_run(output, image=b"original")
            image.write_bytes(b"tampered")

            with self.assertRaisesRegex(runner.RunnerError, "hash"):
                runner.cleanup(argparse.Namespace(output=temp))
            self.assertTrue(image.exists())

    def test_cleanup_refuses_traversal_and_symlink_inventory(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            outside = output.parent / "outside-user.png"
            outside.write_bytes(b"keep")
            runner.write_json(output / "run.json", {"owned_images": [{"path": "../outside-user.png", "kind": "transient", "sha256": runner.sha256(outside)}]})
            with self.assertRaisesRegex(runner.RunnerError, "outside"):
                runner.cleanup(argparse.Namespace(output=temp))
            self.assertTrue(outside.exists())

            link = output / "linked.png"
            link.symlink_to(outside)
            runner.write_json(output / "run.json", {"owned_images": [{"path": "linked.png", "kind": "transient", "sha256": runner.sha256(outside)}]})
            with self.assertRaisesRegex(runner.RunnerError, "symlink"):
                runner.cleanup(argparse.Namespace(output=temp))
            self.assertTrue(outside.exists())

    def test_cleanup_is_idempotent_after_image_deletion(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            case_dir, image = self._reviewed_run(output)
            runner.cleanup(argparse.Namespace(output=temp))
            runner.cleanup(argparse.Namespace(output=temp))
            self.assertFalse(image.exists())
            self.assertTrue(json.loads((case_dir / "result.json").read_text())["image_deleted"])


if __name__ == "__main__":
    unittest.main()
