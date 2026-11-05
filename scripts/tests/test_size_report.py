import io
import json
import sys
import tempfile
import unittest
import zlib
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import size_report


def run(argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = size_report.main(argv)
    return code, out.getvalue(), err.getvalue()


class SizeReportTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        root = Path(self.dir.name)
        self.binary = root / "tablex-gpui"
        # Half incompressible, half zeros: the deflated size is well below the file size.
        self.payload = bytes(range(256)) * 400 + bytes(100_000)
        self.binary.write_bytes(self.payload)
        self.budget = root / "size-budget.json"

    def write_budget(self, **entry):
        self.budget.write_text(json.dumps({"targets": {"test-os": entry}}))

    def args(self, *extra):
        return ["--binary", str(self.binary), "--target", "test-os", "--budget", str(self.budget), *extra]

    def test_measures_the_file_and_its_deflated_size(self):
        measurement = size_report.measure(self.binary, "test-os")
        self.assertEqual(measurement.binary, len(self.payload))
        compressor = zlib.compressobj(9, zlib.DEFLATED, -zlib.MAX_WBITS)
        expected = len(compressor.compress(self.payload) + compressor.flush())
        self.assertEqual(measurement.compressed, expected)
        self.assertLess(measurement.compressed, measurement.binary)

    def test_within_budget_passes_and_reports_headroom(self):
        self.write_budget(binary_max_bytes=len(self.payload) + 1000, compressed_max_bytes=len(self.payload))
        code, out, err = run(self.args("--check"))
        self.assertEqual((code, err), (0, ""))
        self.assertIn("headroom", out)
        self.assertNotIn("OVER BUDGET", out)

    def test_over_budget_fails_only_with_check_and_names_the_metric(self):
        self.write_budget(binary_max_bytes=len(self.payload) - 1, compressed_max_bytes=len(self.payload))
        code, out, err = run(self.args("--check"))
        self.assertEqual(code, 1)
        self.assertIn("OVER BUDGET", out)
        self.assertIn("binary is", err)
        self.assertNotIn("compressed is", err)
        code, _, err = run(self.args())
        self.assertEqual(code, 0, "without --check an overrun is reported, not failed")
        self.assertIn("binary is", err)

    def test_a_target_without_a_budget_is_reported_not_failed(self):
        self.budget.write_text(json.dumps({"targets": {"another-os": {"binary_max_bytes": 1}}}))
        code, out, err = run(self.args("--check"))
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(out.count("no budget"), 2)

    def test_a_missing_budget_file_is_no_budget(self):
        code, out, _ = run(self.args("--check"))
        self.assertEqual(code, 0)
        self.assertIn("no budget", out)

    def test_json_and_markdown_outputs(self):
        self.write_budget(binary_max_bytes=10**9)
        code, out, _ = run(self.args("--json"))
        self.assertEqual(code, 0)
        data = json.loads(out)
        self.assertEqual(data["target"], "test-os")
        self.assertEqual(data["binary"], len(self.payload))
        self.assertEqual(data["budget"], {"binary": 10**9, "compressed": None})
        code, out, _ = run(self.args("--markdown"))
        self.assertEqual(code, 0)
        self.assertIn("| binary |", out)
        self.assertIn("| compressed |", out)
        self.assertIn("not tracked", out)

    def test_bad_inputs_are_reported_with_exit_code_2(self):
        missing = Path(self.dir.name) / "absent"
        code, _, err = run(["--binary", str(missing), "--target", "test-os", "--budget", str(self.budget)])
        self.assertEqual(code, 2)
        self.assertIn("no executable", err)
        self.budget.write_text("{not json")
        code, _, err = run(self.args())
        self.assertEqual(code, 2)
        self.assertIn("not valid JSON", err)
        for bad in (0, -5, "12", True, 1.5):
            self.write_budget(binary_max_bytes=bad)
            code, _, err = run(self.args())
            self.assertEqual(code, 2, bad)
            self.assertIn("positive integer", err)

    def test_another_package_has_its_own_budget_and_default_path(self):
        self.budget.write_text(
            json.dumps(
                {
                    "targets": {"test-os": {"binary_max_bytes": 1}},
                    "packages": {
                        "tablex-mcp": {"targets": {"test-os": {"binary_max_bytes": len(self.payload) - 1}}}
                    },
                }
            )
        )
        code, out, err = run(self.args("--package", "tablex-mcp", "--check"))
        self.assertEqual(code, 1)
        self.assertIn("tablex-mcp (dist profile", out)
        self.assertIn("tablex-mcp binary is", err)
        self.assertIn("no budget", out, "compressed has no limit for this package")
        code, out, _ = run(self.args("--package", "unknown-tool", "--check", "--json"))
        self.assertEqual(code, 0, "a package without an entry is never failed")
        self.assertEqual(json.loads(out)["package"], "unknown-tool")
        self.assertEqual(size_report.default_binary("tablex-mcp").stem, "tablex-mcp")
        self.assertEqual(size_report.default_binary().stem, "tablex-gpui")

    def test_the_checked_in_budget_file_is_valid_for_every_target(self):
        budgets = json.loads(size_report.BUDGET_FILE.read_text())
        self.assertIn("targets", budgets)
        for target in budgets["targets"]:
            size_report.load_budget(size_report.BUDGET_FILE, target)
        for package, entry in budgets.get("packages", {}).items():
            for target in entry["targets"]:
                self.assertTrue(size_report.load_budget(size_report.BUDGET_FILE, target, package))

    def test_this_machine_maps_to_a_known_target_key(self):
        system, _, arch = size_report.current_target().partition("-")
        self.assertIn(system, {"macos", "linux", "windows"})
        self.assertTrue(arch)


if __name__ == "__main__":
    unittest.main()
