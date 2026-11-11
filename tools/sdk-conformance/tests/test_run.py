"""Unit tests for the harness command construction and report mapping (WP-STR-07.T1).

These never touch a container or the contract kit: they exercise the pure pieces of ``run.py``
and its ``--list`` mode.
"""

import contextlib
import io
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import matrix  # noqa: E402
import run  # noqa: E402


class CommandTests(unittest.TestCase):
    def test_container_command_mounts_the_repo(self):
        command = run.container_command(
            "podman", Path("/repo"), "sdk/templates/rust", "rust:1.98", "./build.sh"
        )
        self.assertEqual(command[0], "podman")
        self.assertIn("/repo:/work", command)
        self.assertIn("/work/sdk/templates/rust", command)
        self.assertEqual(command[-3:], ["sh", "-lc", "./build.sh"])

    def test_local_command(self):
        self.assertEqual(run.local_command("./build.sh"), ["sh", "-lc", "./build.sh"])

    def test_runner_command(self):
        command = run.runner_command(
            Path("/r"), Path("/c.wasm"), Path("/cases.json"), "rust", Path("/out.json")
        )
        self.assertEqual(command[0], "/r")
        self.assertIn("--component", command)
        self.assertIn("/c.wasm", command)
        self.assertEqual(command[command.index("--language") + 1], "rust")


class ReportMappingTests(unittest.TestCase):
    def test_a_blocked_report_is_a_blocked_row(self):
        report = {
            "blocked": {"step": "instantiate", "reason": "wasi:io/poll is not provided"},
            "binary_bytes": 100,
            "notes": ["imports: wasi:io/poll"],
        }
        outcome = run.outcome_from_runner_report("python", "python:3.14", "CPython", report)
        self.assertEqual(outcome.status, matrix.BLOCKED)
        self.assertEqual(
            outcome.render_status(), "BLOCKED(instantiate, wasi:io/poll is not provided)"
        )

    def test_a_passing_report_is_supported(self):
        report = {
            "passed": True,
            "binary_bytes": 10,
            "cases": [{"name": "a", "passed": True}],
        }
        outcome = run.outcome_from_runner_report("rust", "rust:1.98", "rustc", report)
        self.assertEqual(outcome.status, matrix.SUPPORTED)

    def test_a_failing_report_is_failed(self):
        report = {"passed": False, "cases": [{"name": "a", "passed": False}]}
        outcome = run.outcome_from_runner_report("go", "golang", "go", report)
        self.assertEqual(outcome.status, matrix.FAILED)

    def test_a_pass_with_no_cases_is_failed(self):
        report = {"passed": True, "cases": []}
        outcome = run.outcome_from_runner_report("go", "golang", "go", report)
        self.assertEqual(outcome.status, matrix.FAILED)


class EngineTests(unittest.TestCase):
    def test_local_is_returned_as_is(self):
        self.assertEqual(run._detect_engine("local"), "local")

    def test_auto_detects_something(self):
        self.assertIn(run._detect_engine("auto"), ("podman", "docker", "none"))


class ArgsTests(unittest.TestCase):
    def test_defaults(self):
        args = run.parse_args([])
        self.assertFalse(args.run)
        self.assertFalse(args.list)
        self.assertEqual(args.engine, "auto")
        self.assertEqual(args.lang, [])

    def test_lang_repeats(self):
        args = run.parse_args(["--run", "--lang", "rust", "--lang", "go"])
        self.assertEqual(args.lang, ["rust", "go"])

    def test_list_and_run_are_exclusive(self):
        with self.assertRaises(SystemExit):
            run.parse_args(["--list", "--run"])


class ListModeTests(unittest.TestCase):
    def test_list_prints_the_matrix_and_exits_zero(self):
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            code = run.main(["--list"])
        self.assertEqual(code, 0)
        output = buffer.getvalue()
        self.assertIn("Guest SDK conformance matrix", output)
        for language in ("rust", "go", "csharp", "javascript", "python"):
            self.assertIn(language, output)
        self.assertIn("contract cases", output)

    def test_an_unknown_language_is_a_usage_error(self):
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer), contextlib.redirect_stderr(io.StringIO()):
            code = run.main(["--run", "--lang", "cobol"])
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
