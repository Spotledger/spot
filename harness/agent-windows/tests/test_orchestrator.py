"""Tests for the orchestration logic with a stubbed runner (WP-ADM-05.T3).

The real Windows execution cannot happen on the Linux CI host, so the runner is stubbed
and the orchestration around it -- capability gating, selection, aggregation, JUnit,
rendering and the ``pwsh`` argument vector -- is tested here for real.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import orchestrator  # noqa: E402
from orchestrator import (  # noqa: E402
    STATUS_ERROR,
    STATUS_FAIL,
    STATUS_PASS,
    STATUS_SKIP,
    MatrixError,
    PwshRunner,
    RunOutcome,
    load_matrix,
    run_matrix,
)

RUNNER_JUNIT = """\
<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="windows-11/services" tests="2" failures="0" skipped="0">
    <testcase classname="windows-11.services" name="throwaway_service_lifecycle" />
    <testcase classname="windows-11.services" name="enumeration_is_prefixed" />
  </testsuite>
</testsuites>
"""


class StubRunner:
    """A runner that never touches Windows: it records calls and returns canned results."""

    def __init__(self, statuses: dict[tuple[str, str], str] | None = None, default: str = STATUS_PASS):
        self.statuses = statuses or {}
        self.default = default
        self.calls: list[tuple[str, str]] = []

    def run(self, target, test) -> RunOutcome:
        self.calls.append((target.name, test.name))
        status = self.statuses.get((target.name, test.name), self.default)
        passed = 1 if status == STATUS_PASS else 0
        failed = 1 if status == STATUS_FAIL else 0
        return RunOutcome(
            target=target.name,
            test=test.name,
            status=status,
            passed=passed,
            failed=failed,
            message="" if status == STATUS_PASS else "boom",
        )


class TestRunMatrix(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.matrix = load_matrix()

    def test_all_pairs_run_and_pass(self) -> None:
        runner = StubRunner()
        report = run_matrix(self.matrix, runner)
        self.assertTrue(report.ok)
        self.assertEqual(len(runner.calls), len(self.matrix.targets) * len(self.matrix.tests))
        self.assertEqual(report.counts()[STATUS_PASS], len(runner.calls))

    def test_one_failure_makes_the_report_not_ok(self) -> None:
        runner = StubRunner({("windows-11", "vss"): STATUS_FAIL})
        report = run_matrix(self.matrix, runner)
        self.assertFalse(report.ok)
        self.assertEqual(report.counts()[STATUS_FAIL], 1)

    def test_an_error_also_makes_the_report_not_ok(self) -> None:
        runner = StubRunner({("windows-10-22h2", "users"): STATUS_ERROR})
        report = run_matrix(self.matrix, runner)
        self.assertFalse(report.ok)
        self.assertEqual(report.counts()[STATUS_ERROR], 1)

    def test_a_test_whose_capabilities_are_missing_is_skipped_not_run(self) -> None:
        # A windows-11 target without the vss capability: the vss test must be skipped and
        # the runner must never be asked to run it, so a machine that cannot do VSS cannot
        # report a false pass.
        crippled = dataclasses.replace(
            self.matrix.target("windows-11"),
            capabilities=tuple(c for c in self.matrix.target("windows-11").capabilities if c != "vss"),
        )
        matrix = dataclasses.replace(
            self.matrix,
            targets=tuple(crippled if t.name == "windows-11" else t for t in self.matrix.targets),
        )
        runner = StubRunner()
        report = run_matrix(matrix, runner, targets=["windows-11"], tests=["vss", "services"])
        skipped = [o for o in report.outcomes if o.status == STATUS_SKIP]
        self.assertEqual(len(skipped), 1)
        self.assertEqual(skipped[0].test, "vss")
        self.assertIn("vss", skipped[0].message)
        self.assertEqual(runner.calls, [("windows-11", "services")])
        self.assertTrue(report.ok, "a skip is allowed; only a failure blocks")

    def test_selection_limits_the_matrix(self) -> None:
        runner = StubRunner()
        report = run_matrix(
            self.matrix, runner, targets=["windows-server-2025"], tests=["services", "firewall"]
        )
        self.assertEqual(len(report.outcomes), 2)
        self.assertEqual(
            runner.calls, [("windows-server-2025", "services"), ("windows-server-2025", "firewall")]
        )

    def test_unknown_selection_raises(self) -> None:
        with self.assertRaises(MatrixError):
            run_matrix(self.matrix, StubRunner(), targets=["windows-7"])

    def test_on_outcome_is_called_for_every_pair(self) -> None:
        seen: list[RunOutcome] = []
        run_matrix(self.matrix, StubRunner(), targets=["windows-11"], on_outcome=seen.append)
        self.assertEqual(len(seen), len(self.matrix.tests))


class TestReportViews(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.matrix = load_matrix()

    def test_render_has_a_row_per_pair_and_a_summary(self) -> None:
        report = run_matrix(
            self.matrix,
            StubRunner({("windows-11", "vss"): STATUS_FAIL}),
            targets=["windows-11"],
            tests=["services", "vss"],
        )
        text = report.render()
        self.assertIn("TARGET", text)
        self.assertIn("windows-11", text)
        self.assertIn("services", text)
        self.assertIn("1 passed", text)
        self.assertIn("1 failed", text)

    def test_to_junit_is_well_formed_and_counts_correctly(self) -> None:
        report = run_matrix(
            self.matrix,
            StubRunner({("windows-11", "vss"): STATUS_FAIL}),
            targets=["windows-11"],
            tests=["services", "vss"],
        )
        root = ET.fromstring(report.to_junit())
        self.assertEqual(root.tag, "testsuites")
        suites = root.findall("testsuite")
        self.assertEqual(len(suites), 1)
        self.assertEqual(suites[0].get("tests"), "2")
        self.assertEqual(suites[0].get("failures"), "1")
        cases = suites[0].findall("testcase")
        self.assertEqual(len(cases), 2)

    def test_to_junit_keeps_a_runners_own_cases(self) -> None:
        outcome = RunOutcome(
            target="windows-11",
            test="services",
            status=STATUS_PASS,
            passed=2,
            junit=RUNNER_JUNIT,
        )
        report = orchestrator.MatrixReport(outcomes=(outcome,))
        root = ET.fromstring(report.to_junit())
        cases = root.findall(".//testcase")
        self.assertEqual(len(cases), 2)
        self.assertEqual(cases[0].get("name"), "throwaway_service_lifecycle")

    def test_to_junit_synthesises_a_skipped_case(self) -> None:
        outcome = RunOutcome(
            target="windows-11",
            test="vss",
            status=STATUS_SKIP,
            message="missing capabilities: vss",
        )
        report = orchestrator.MatrixReport(outcomes=(outcome,))
        root = ET.fromstring(report.to_junit())
        skipped = root.findall(".//skipped")
        self.assertEqual(len(skipped), 1)
        self.assertEqual(skipped[0].get("message"), "missing capabilities: vss")

    def test_to_junit_groups_by_target(self) -> None:
        report = run_matrix(
            self.matrix, StubRunner(), targets=["windows-11", "windows-server-2022"], tests=["services"]
        )
        root = ET.fromstring(report.to_junit())
        names = {suite.get("name") for suite in root.findall("testsuite")}
        self.assertEqual(names, {"windows-11", "windows-server-2022"})


class TestPwshRunner(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.matrix = load_matrix()

    def test_command_is_an_argument_vector(self) -> None:
        runner = PwshRunner(artifacts=Path("/tmp/artifacts"))
        command = runner.command(self.matrix.target("windows-11"), self.matrix.real_test("services"))
        self.assertEqual(command[0], "pwsh")
        self.assertIn("-NoProfile", command)
        self.assertIn("-Target", command)
        self.assertEqual(command[command.index("-Target") + 1], "windows-11")
        self.assertEqual(command[command.index("-Test") + 1], "services")
        self.assertEqual(command[command.index("-Artifacts") + 1], "/tmp/artifacts")
        self.assertIn("-Json", command)

    def test_run_reads_the_json_outcome_and_its_junit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            junit_path = Path(directory) / "services.xml"
            junit_path.write_text(RUNNER_JUNIT, encoding="utf-8")
            payload = {
                "target": "windows-11",
                "test": "services",
                "status": STATUS_PASS,
                "passed": 2,
                "failed": 0,
                "skipped": 0,
                "message": "",
                "junit_path": str(junit_path),
            }
            completed = subprocess.CompletedProcess(
                args=["pwsh"], returncode=0, stdout="noise\n" + json.dumps(payload) + "\n", stderr=""
            )
            with mock.patch.object(orchestrator.subprocess, "run", return_value=completed):
                outcome = PwshRunner().run(
                    self.matrix.target("windows-11"), self.matrix.real_test("services")
                )
        self.assertEqual(outcome.status, STATUS_PASS)
        self.assertEqual(outcome.passed, 2)
        self.assertIn("throwaway_service_lifecycle", outcome.junit)

    def test_run_without_json_is_an_error_not_a_pass(self) -> None:
        completed = subprocess.CompletedProcess(args=["pwsh"], returncode=1, stdout="", stderr="nope")
        with mock.patch.object(orchestrator.subprocess, "run", return_value=completed):
            outcome = PwshRunner().run(
                self.matrix.target("windows-11"), self.matrix.real_test("services")
            )
        self.assertEqual(outcome.status, STATUS_ERROR)
        self.assertFalse(outcome.status == STATUS_PASS)

    def test_last_json_object_skips_noise(self) -> None:
        self.assertEqual(
            orchestrator._last_json_object('warning\n{"a": 1}\ntrailing'), {"a": 1}
        )
        self.assertIsNone(orchestrator._last_json_object("no json here"))
        self.assertIsNone(orchestrator._last_json_object('{"broken": '))


class TestCommandLine(unittest.TestCase):
    def test_matrix_json_command(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(orchestrator.HARNESS_DIR / "orchestrator.py"), "matrix", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        view = json.loads(completed.stdout)
        self.assertEqual(view["module"], "spot-agent-windows")
        self.assertEqual(len(view["targets"]), 4)

    def test_matrix_text_command(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(orchestrator.HARNESS_DIR / "orchestrator.py"), "matrix"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Windows module conformance matrix", completed.stdout)
        self.assertIn("windows-11", completed.stdout)

    def test_run_command_drives_the_runner(self) -> None:
        # The CLI must fail (non-zero) when the runner reports a failure, and must not
        # pretend a missing pwsh is a pass.
        with mock.patch.object(
            orchestrator,
            "PwshRunner",
            lambda **kwargs: StubRunner({("windows-11", "services"): STATUS_FAIL}),
        ):
            code = orchestrator.main(["run", "--target", "windows-11", "--test", "services"])
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
