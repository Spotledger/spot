"""Tests for the libtest parser and the JUnit XML writer."""

import sys
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from agent_linux_harness.junit import (  # noqa: E402
    ERROR,
    FAILED,
    PASSED,
    SKIPPED,
    TargetReport,
    TestResult,
    parse_libtest_output,
    render_junit,
    run_duration,
)

PASSING = """\
running 3 tests
test alpha ... ok
test beta ... ok
test gamma ... ignored

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.04s
"""

FAILING = """\
running 2 tests
test alpha ... ok
test beta ... FAILED

failures:

---- beta stdout ----
thread 'beta' panicked at tests/real.rs:10:5:
assertion failed: the daemon is running
note: run with `RUST_BACKTRACE=1` for a backtrace

failures:
    beta

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
"""


class ParseTests(unittest.TestCase):
    def test_parses_ok_and_ignored(self):
        results = parse_libtest_output(PASSING)
        self.assertEqual(results["alpha"].status, PASSED)
        self.assertEqual(results["beta"].status, PASSED)
        self.assertEqual(results["gamma"].status, SKIPPED)

    def test_parses_a_failure_with_detail(self):
        results = parse_libtest_output(FAILING)
        self.assertEqual(results["alpha"].status, PASSED)
        self.assertEqual(results["beta"].status, FAILED)
        self.assertIn("the daemon is running", results["beta"].detail)

    def test_run_duration_is_read(self):
        self.assertAlmostEqual(run_duration(FAILING), 0.12)
        self.assertEqual(run_duration("no summary here"), 0.0)

    def test_ran_flag(self):
        self.assertTrue(TestResult("a", PASSED).ran)
        self.assertFalse(TestResult("a", SKIPPED).ran)


class RenderTests(unittest.TestCase):
    def test_render_produces_valid_junit(self):
        report = TargetReport(
            "fedora-44",
            [
                TestResult("alpha", PASSED, duration=0.01),
                TestResult("beta", FAILED, "boom", 0.02),
                TestResult("gamma", SKIPPED, "needs vm"),
            ],
        )
        xml = render_junit([report])
        root = ET.fromstring(xml)
        self.assertEqual(root.tag, "testsuites")
        suite = root.find("testsuite")
        self.assertEqual(suite.get("name"), "fedora-44")
        self.assertEqual(suite.get("tests"), "3")
        self.assertEqual(suite.get("failures"), "1")
        self.assertEqual(suite.get("skipped"), "1")
        cases = suite.findall("testcase")
        self.assertEqual([case.get("name") for case in cases], ["alpha", "beta", "gamma"])
        self.assertIsNotNone(cases[1].find("failure"))
        self.assertIsNotNone(cases[2].find("skipped"))

    def test_render_includes_a_setup_error(self):
        report = TargetReport("ubuntu-2404", [TestResult("alpha", ERROR, "pull failed")])
        report.error = "pull failed"
        root = ET.fromstring(render_junit([report]))
        self.assertIsNotNone(root.find("testsuite").find("testcase").find("error"))

    def test_two_targets_become_two_suites(self):
        reports = [TargetReport("a", []), TargetReport("b", [])]
        root = ET.fromstring(render_junit(reports))
        self.assertEqual(len(root.findall("testsuite")), 2)


if __name__ == "__main__":
    unittest.main()
