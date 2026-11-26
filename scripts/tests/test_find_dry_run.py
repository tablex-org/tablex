import io
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import find_dry_run

SHA = "a" * 40
OTHER = "b" * 40
NEED = ["app-intel", "app-windows-aarch64"]


def run(run_id, sha=SHA, created="2026-11-25T00:00:00Z"):
    return {"databaseId": run_id, "headSha": sha, "createdAt": created}


class FindRunTest(unittest.TestCase):
    def find(self, runs, artifacts, required=NEED, sha=SHA):
        with redirect_stderr(io.StringIO()):
            return find_dry_run.find_run(runs, lambda run_id: set(artifacts.get(run_id, ())), sha, required)

    def test_the_newest_run_of_the_commit_wins(self):
        runs = [run(1, created="2026-11-24T00:00:00Z"), run(2, created="2026-11-25T00:00:00Z")]
        self.assertEqual(self.find(runs, {1: NEED, 2: NEED}), 2)

    def test_a_run_of_another_commit_is_not_reused(self):
        self.assertIsNone(self.find([run(1, sha=OTHER)], {1: NEED}))

    def test_a_run_without_every_artifact_is_skipped_for_an_older_complete_one(self):
        runs = [run(1, created="2026-11-24T00:00:00Z"), run(2, created="2026-11-25T00:00:00Z")]
        self.assertEqual(self.find(runs, {1: NEED, 2: NEED[:1]}), 1)

    def test_expired_artifacts_are_missing_artifacts(self):
        self.assertIsNone(self.find([run(1)], {1: []}))

    def test_no_runs(self):
        self.assertIsNone(self.find([], {}))


class MainTest(unittest.TestCase):
    def main(self, gh_outputs, sha=SHA):
        """Runs main() with `gh` answering from `gh_outputs` (a callable of its arguments)."""
        with tempfile.TemporaryDirectory() as tmp:
            github_output = Path(tmp) / "output"
            argv = ["--repo", "o/r", "--workflow", "release-app.yml", "--sha", sha, "--require", *NEED]
            out, err = io.StringIO(), io.StringIO()
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(github_output)}), \
                    mock.patch.object(find_dry_run, "gh", side_effect=lambda *a: gh_outputs(a)), \
                    redirect_stdout(out), redirect_stderr(err):
                code = find_dry_run.main(argv)
            written = github_output.read_text() if github_output.exists() else ""
            return code, written, err.getvalue()

    @staticmethod
    def gh_with(runs_json, artifacts):
        def answer(args):
            if args[0] == "run":
                return runs_json
            run_id = int(args[2].split("/")[5])
            return "\n".join(artifacts[run_id]) + "\n"
        return answer

    def test_writes_the_run_id(self):
        runs = f'[{{"databaseId": 7, "headSha": "{SHA}", "createdAt": "2026-11-25T00:00:00Z"}}]'
        code, written, _ = self.main(self.gh_with(runs, {7: NEED}))
        self.assertEqual((code, written), (0, "run_id=7\n"))

    def test_writes_an_empty_id_when_nothing_is_reusable(self):
        code, written, _ = self.main(self.gh_with("[]", {}))
        self.assertEqual((code, written), (0, "run_id=\n"))

    def test_a_gh_failure_is_a_warning_and_an_empty_id(self):
        def fail(args):
            raise subprocess.CalledProcessError(1, ["gh"], stderr="HTTP 403")
        code, written, err = self.main(fail)
        self.assertEqual((code, written), (0, "run_id=\n"))
        self.assertIn("::warning::", err)

    def test_a_short_sha_is_refused(self):
        code, written, _ = self.main(self.gh_with("[]", {}), sha="abc")
        self.assertEqual((code, written), (2, ""))


if __name__ == "__main__":
    unittest.main()
