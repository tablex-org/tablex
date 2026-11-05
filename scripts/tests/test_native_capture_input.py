#!/usr/bin/env python3
"""Opt-in end-to-end proof that the native helper reaches AppKit."""

import json
import os
from pathlib import Path
import shutil
import signal
import select
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "scripts" / "gpui-capture-helper.swift"
PROBE = Path(__file__).with_name("native-input-probe.swift")
TITLE = "TableX UI Scenario: new-connection"
SHIFT_FLAG = 1 << 17
COMMAND_FLAG = 1 << 20


class NativeCaptureInputTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.environ.get("TABLEX_NATIVE_CAPTURE_TESTS") != "1":
            raise unittest.SkipTest("set TABLEX_NATIVE_CAPTURE_TESTS=1 to run native macOS tests")
        if os.uname().sysname != "Darwin" or shutil.which("swiftc") is None:
            raise unittest.SkipTest("requires macOS and swiftc")
        cls.temp = tempfile.TemporaryDirectory(prefix="tablex-native-input-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.dir = Path(cls.temp.name)
        cls.probe_bin = cls.dir / "native-input-probe"
        cls.helper_bin = cls.dir / "native-input-helper"
        for source, output in ((PROBE, cls.probe_bin), (HELPER, cls.helper_bin)):
            result = subprocess.run(["swiftc", "-O", "-warnings-as-errors", "-framework", "AppKit", "-framework", "ApplicationServices", "-framework", "CoreGraphics", str(source), "-o", str(output)], text=True, capture_output=True)
            if result.returncode:
                raise AssertionError(f"swiftc failed for {source}:\n{result.stdout}\n{result.stderr}")
        environment = json.loads(subprocess.check_output([str(cls.helper_bin), "environment"], text=True))
        if environment.get("locked") or not environment.get("desktop_available") or not environment.get("input_allowed"):
            cls.temp.cleanup()
            raise unittest.SkipTest("native UI input requires an unlocked desktop with Accessibility permission")
        cls.proc = subprocess.Popen([str(cls.probe_bin)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        cls.addClassCleanup(cls.stop_probe)
        cls.events = []
        cls.pending = ""
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            ready, _, _ = select.select([cls.proc.stdout], [], [], max(0, deadline - time.monotonic()))
            if ready:
                cls.pending += os.read(cls.proc.stdout.fileno(), 65536).decode()
                while "\n" in cls.pending:
                    line, cls.pending = cls.pending.split("\n", 1)
                    if not line:
                        continue
                    value = json.loads(line)
                    cls.events.append(value)
                    if value.get("event") == "ready":
                        cls.ready = value
                        return
        raise AssertionError("probe did not emit ready")

    @classmethod
    def stop_probe(cls):
        if hasattr(cls, "proc"):
            if cls.proc.poll() is None:
                cls.proc.send_signal(signal.SIGTERM)
            try:
                cls.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                cls.proc.kill()
                cls.proc.wait()
            cls.proc.stdout.close()
            cls.proc.stderr.close()

    @classmethod
    def invoke(cls, *args, stdin=None):
        result = subprocess.run([str(cls.helper_bin), *args], input=stdin, text=True, capture_output=True)
        if result.returncode:
            raise AssertionError(f"helper failed ({result.returncode}): {result.stderr or result.stdout}")
        return json.loads(result.stdout)

    @classmethod
    def drain_events(cls, seconds=1):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([cls.proc.stdout], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                return
            cls.pending += os.read(cls.proc.stdout.fileno(), 65536).decode()
            while "\n" in cls.pending:
                line, cls.pending = cls.pending.split("\n", 1)
                if line:
                    cls.events.append(json.loads(line))

    def test_native_click_tab_and_unicode_are_received_once(self):
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", "TableX UI Scenario: new-connection")
        self.invoke("click", *args, "--x", "100", "--y", "200")
        self.invoke("key", *args, "--key", "tab")
        self.invoke("text", *args, "--text-stdin", stdin="A🙂é")
        self.drain_events()
        received = [event for event in self.events if event.get("event") == "received"]
        mouse = [event for event in received if event.get("kind") == "mouse"]
        keys = [event for event in received if event.get("kind") == "key" and event.get("keyCode") in {5, 123}]
        self.assertEqual([event["phase"] for event in mouse], ["down", "up"])
        self.assertTrue(all(event["windowNumber"] == self.ready["windowNumber"] for event in mouse))
        self.assertEqual([(event["localX"], event["localY"]) for event in mouse], [(100, self.ready["windowHeight"] - 200)] * 2)
        self.assertEqual([event["phase"] for event in keys[:2]], ["down", "up"])
        self.assertEqual(keys[2]["unicodeLength"], 4)
        self.assertEqual(len(mouse), 2)
        self.assertEqual(len(keys), 4)
        self.assertTrue(all(event["windowNumber"] == self.ready["windowNumber"] for event in keys))

    def test_native_modifier_keys_and_click_preserve_flags(self):
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", TITLE)
        before = len(self.events)
        for key in ("shift-left", "shift-right", "shift-up", "shift-down"):
            self.invoke("key", *args, "--key", key)
        self.invoke("key", *args, "--key", "cmd-shift-g")
        self.invoke("click", *args, "--x", "100", "--y", "200", "--modifier", "shift")
        self.invoke("click", *args, "--x", "120", "--y", "220", "--modifier", "cmd")
        self.drain_events()
        received = [event for event in self.events[before:] if event.get("event") == "received"]
        keys = [event for event in received if event.get("kind") == "key"]
        mouse = [event for event in received if event.get("kind") == "mouse"]
        shift_arrows = [event for event in keys if event["keyCode"] in {123, 124, 125, 126}]
        cmd_shift_g = [event for event in keys if event["keyCode"] == 5]
        self.assertEqual({event["keyCode"] for event in shift_arrows if event["phase"] == "down"}, {123, 124, 125, 126})
        self.assertTrue(any(event["phase"] == "down" for event in cmd_shift_g), cmd_shift_g)
        self.assertTrue(all(event["modifierFlags"] & SHIFT_FLAG for event in shift_arrows))
        self.assertTrue(all(event["modifierFlags"] & (SHIFT_FLAG | COMMAND_FLAG) == (SHIFT_FLAG | COMMAND_FLAG) for event in cmd_shift_g))
        self.assertEqual([event["phase"] for event in mouse], ["down", "up", "down", "up"])
        self.assertTrue(all(event["modifierFlags"] & SHIFT_FLAG for event in mouse[:2]))
        self.assertTrue(all(event["modifierFlags"] & COMMAND_FLAG for event in mouse[2:]))

    def test_rejects_wrong_path_and_secondary_window_without_delivering_input(self):
        self.drain_events(0.1)
        before = len(self.events)
        for path, window in (("/not/the/owned/process", self.ready["windowNumber"]), (str(self.probe_bin), self.ready["secondaryWindowNumber"])):
            result = subprocess.run([str(self.helper_bin), "key", "--pid", str(self.ready["pid"]), "--path", path, "--window-id", str(window), "--title", "TableX UI Scenario: new-connection", "--key", "tab"], text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
        self.drain_events(0.3)
        self.assertEqual(len(self.events), before)

    def test_unicode_chunk_boundary_does_not_split_surrogates(self):
        self.drain_events(0.1)
        before = len(self.events)
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", "TableX UI Scenario: new-connection")
        self.invoke("text", *args, "--text-stdin", stdin="x" * 19 + "🙂" + "y")
        self.drain_events()
        keys = [event for event in self.events[before:] if event.get("kind") == "key" and event.get("phase") == "down"]
        self.assertEqual([event["unicodeLength"] for event in keys], [19, 3])
        self.assertFalse(any(event["replacementCharacter"] for event in keys))

    def test_z_double_click_uses_native_click_counts(self):
        self.drain_events(0.1)
        before = len(self.events)
        self.invoke("click", "--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", "TableX UI Scenario: new-connection", "--x", "100", "--y", "200", "--click-count", "2")
        self.drain_events()
        mouse = [event for event in self.events[before:] if event.get("kind") == "mouse"]
        self.assertEqual([event["phase"] for event in mouse], ["down", "up", "down", "up"])
        self.assertEqual([event["clickCount"] for event in mouse], [1, 1, 2, 2])

    def test_native_right_click_and_drag_preserve_order_button_target_and_coordinates(self):
        self.drain_events(0.1)
        before = len(self.events)
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", TITLE)
        self.invoke("right-click", *args, "--x", "100", "--y", "200")
        self.invoke("drag", *args, "--x1", "100", "--y1", "200", "--x2", "300", "--y2", "400")
        self.drain_events()
        received = [event for event in self.events[before:] if event.get("event") == "received"]
        right = [event for event in received if event.get("kind") == "right"]
        drag = [event for event in received if event.get("kind") in {"mouse", "drag"}]
        self.assertEqual([event["phase"] for event in right], ["down", "up"])
        self.assertTrue(all(event["buttonNumber"] == 1 for event in right))
        self.assertTrue(all(event["clickCount"] == 1 for event in right))
        self.assertEqual([(event["localX"], event["localY"]) for event in right], [(100, self.ready["windowHeight"] - 200)] * 2)
        self.assertEqual([event["kind"] for event in drag], ["mouse", "drag", "drag", "drag", "drag", "drag", "drag", "drag", "drag", "mouse"])
        self.assertEqual([event["phase"] for event in drag], ["down"] + ["drag"] * 8 + ["up"])
        self.assertTrue(all(event["buttonNumber"] == 0 for event in drag))
        self.assertTrue(all(event["clickCount"] == 1 for event in drag))
        self.assertTrue(all(event["windowNumber"] == self.ready["windowNumber"] for event in right + drag))
        self.assertEqual((drag[0]["localX"], drag[0]["localY"]), (100, self.ready["windowHeight"] - 200))
        self.assertEqual((drag[-1]["localX"], drag[-1]["localY"]), (300, self.ready["windowHeight"] - 400))

    def test_native_mouse_move_trace_has_no_button_events(self):
        self.drain_events(0.1)
        before = len(self.events)
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", TITLE)
        result = self.invoke("mouse-move-trace", *args, stdin=json.dumps({"points": [[100, 200], [300, 400]], "hz": 60}))
        self.drain_events(0.3)
        received = [event for event in self.events[before:] if event.get("event") == "received"]
        moves = [event for event in received if event.get("kind") == "move"]
        self.assertEqual(result["event_count"], 2)
        self.assertTrue(moves, "the target window must receive a mouse-move event")
        self.assertEqual((moves[-1]["localX"], moves[-1]["localY"]), (300, self.ready["windowHeight"] - 400))
        self.assertEqual((moves[-1]["cursorX"], moves[-1]["cursorY"]), (300, self.ready["windowHeight"] - 400))
        self.assertTrue(all(event["windowNumber"] == self.ready["windowNumber"] and event["buttonNumber"] == 0 for event in moves))
        self.assertFalse(any(event.get("kind") in {"mouse", "right", "drag"} for event in received))

    def test_native_mouse_move_trace_rejects_outside_coordinates(self):
        self.drain_events(0.1)
        before = len(self.events)
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", TITLE)
        result = subprocess.run([str(self.helper_bin), "mouse-move-trace", *args], input=json.dumps({"points": [[641, 200]], "hz": 60}), text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.drain_events(0.2)
        self.assertEqual(len(self.events), before)

    def test_native_mouse_input_rejects_coordinates_outside_owned_window(self):
        self.drain_events(0.1)
        before = len(self.events)
        args = ("--pid", str(self.ready["pid"]), "--path", str(self.probe_bin), "--window-id", str(self.ready["windowNumber"]), "--title", TITLE)
        result = subprocess.run([str(self.helper_bin), "right-click", *args, "--x", "641", "--y", "200"], text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.drain_events(0.3)
        self.assertEqual(len(self.events), before)


if __name__ == "__main__":
    unittest.main()
