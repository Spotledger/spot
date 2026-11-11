"""Unit tests for the matrix model (WP-STR-07.T1). No containers, no Rust: pure data."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import matrix  # noqa: E402


def ok_case(name="a"):
    """A passing case."""
    return {"name": name, "expected": "ok", "actual": "ok", "passed": True, "detail": ""}


class BlockedStatusTests(unittest.TestCase):
    def test_renders_the_brief_form(self):
        self.assertEqual(
            matrix.Blocked("instantiate", "WASI is not provided").render(),
            "BLOCKED(instantiate, WASI is not provided)",
        )

    def test_round_trips_through_text(self):
        blocked = matrix.Blocked("build", "the image is not pinned")
        status, parsed = matrix.parse_status(blocked.render())
        self.assertEqual(status, matrix.BLOCKED)
        self.assertEqual(parsed, blocked)

    def test_parses_the_plain_statuses(self):
        for status in (matrix.SUPPORTED, matrix.FAILED, matrix.NOT_RUN):
            self.assertEqual(matrix.parse_status(status), (status, None))

    def test_rejects_an_unknown_status(self):
        with self.assertRaises(ValueError):
            matrix.parse_status("MAYBE")

    def test_rejects_a_blocked_status_without_a_reason(self):
        with self.assertRaises(ValueError):
            matrix.parse_status("BLOCKED(instantiate)")

    def test_rejects_a_blocked_status_without_a_step(self):
        with self.assertRaises(ValueError):
            matrix.parse_status("BLOCKED(, why)")

    def test_a_reason_may_contain_commas(self):
        status, parsed = matrix.parse_status("BLOCKED(build, one, two, three)")
        self.assertEqual(status, matrix.BLOCKED)
        self.assertEqual(parsed, matrix.Blocked("build", "one, two, three"))


class OutcomeTests(unittest.TestCase):
    def test_counts_cases(self):
        outcome = matrix.Outcome(
            language="rust",
            status=matrix.SUPPORTED,
            cases=[ok_case("a"), {"name": "b", "passed": False}],
        )
        self.assertEqual(outcome.cases_total, 2)
        self.assertEqual(outcome.cases_passed, 1)

    def test_rejects_an_unknown_status(self):
        with self.assertRaises(ValueError):
            matrix.Outcome(language="rust", status="NOPE")

    def test_json_round_trip(self):
        outcome = matrix.Outcome(
            language="go",
            status=matrix.BLOCKED,
            image="golang:1.27",
            binary_bytes=1234,
            blocked=matrix.Blocked("build", "no componentize-go"),
            cases=[],
            notes=["desk review"],
        )
        restored = matrix.Outcome.from_dict(outcome.to_dict())
        self.assertEqual(restored, outcome)

    def test_blocked_helper_sets_the_row(self):
        outcome = matrix.blocked("csharp", "build", "dotnet missing")
        self.assertEqual(outcome.status, matrix.BLOCKED)
        self.assertEqual(outcome.blocked, matrix.Blocked("build", "dotnet missing"))


class OverallTests(unittest.TestCase):
    def test_passes_when_reference_supported_and_blocks_explained(self):
        outcomes = [
            matrix.Outcome("rust", matrix.SUPPORTED, cases=[ok_case()]),
            matrix.blocked("go", "build", "no toolchain"),
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertTrue(passed, problems)

    def test_fails_without_the_reference(self):
        outcomes = [matrix.blocked("go", "build", "no toolchain")]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)
        self.assertTrue(any("reference language" in problem for problem in problems))

    def test_fails_when_the_reference_is_blocked(self):
        outcomes = [
            matrix.blocked("rust", "build", "no wasm-tools"),
            matrix.blocked("go", "build", "no toolchain"),
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)
        self.assertTrue(any("rust" in problem for problem in problems))

    def test_fails_when_a_supported_row_has_no_cases(self):
        outcomes = [matrix.Outcome("rust", matrix.SUPPORTED, cases=[])]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)
        self.assertTrue(any("no contract cases" in problem for problem in problems))

    def test_fails_when_a_supported_case_failed(self):
        outcomes = [
            matrix.Outcome(
                "rust", matrix.SUPPORTED, cases=[{"name": "x", "passed": False}]
            )
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)
        self.assertTrue(any("x" in problem for problem in problems))

    def test_fails_when_a_blocked_row_has_no_reason(self):
        outcomes = [
            matrix.Outcome("rust", matrix.SUPPORTED, cases=[ok_case()]),
            matrix.Outcome("go", matrix.BLOCKED, blocked=matrix.Blocked("build", "")),
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)

    def test_allows_not_run_rows(self):
        outcomes = [
            matrix.Outcome("rust", matrix.SUPPORTED, cases=[ok_case()]),
            matrix.Outcome("python", matrix.NOT_RUN),
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertTrue(passed, problems)

    def test_fails_on_a_failed_row(self):
        outcomes = [
            matrix.Outcome("rust", matrix.SUPPORTED, cases=[ok_case()]),
            matrix.Outcome("python", matrix.FAILED, cases=[{"name": "x", "passed": False}]),
        ]
        passed, problems = matrix.overall(outcomes)
        self.assertFalse(passed)


class RenderingTests(unittest.TestCase):
    def test_markdown_has_one_row_per_language(self):
        outcomes = [
            matrix.Outcome(
                "rust",
                matrix.SUPPORTED,
                image="rust:1.98",
                binary_bytes=34680,
                cases=[ok_case("a"), ok_case("b")],
            ),
            matrix.blocked("go", "build", "no toolchain", image="golang:1.27"),
        ]
        table = matrix.markdown_table(outcomes)
        self.assertIn("| Language | Status |", table)
        self.assertIn("rust", table)
        self.assertIn("SUPPORTED", table)
        self.assertIn("2/2", table)
        self.assertIn("BLOCKED(build, no toolchain)", table)

    def test_markdown_escapes_pipes(self):
        outcome = matrix.blocked("go", "build", "a | b")
        table = matrix.markdown_table([outcome])
        self.assertIn("a \\| b", table)

    def test_json_carries_the_overall_verdict(self):
        text = matrix.to_json([matrix.Outcome("rust", matrix.SUPPORTED, cases=[ok_case()])])
        self.assertIn('"passed": true', text)
        restored = matrix.from_json(text)
        self.assertEqual(len(restored), 1)
        self.assertEqual(restored[0].language, "rust")


if __name__ == "__main__":
    unittest.main()
