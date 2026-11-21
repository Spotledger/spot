"""The JSON report: stable schema, validated before it is written.

The nightly job uploads this file, so a malformed report must never be written: the writer
validates first and refuses, and the validator is exercised against every way a report could
be wrong.
"""

import json
import tempfile
import unittest
from pathlib import Path

import support  # noqa: F401 - puts the driver modules on sys.path

import report


class RowTests(unittest.TestCase):
    def test_a_row_has_the_documented_shape(self):
        row = report.make("fedora-44", "clean_install", report.PASS, 1234, "installed")
        self.assertEqual(
            row.row(),
            {
                "distro": "fedora-44",
                "scenario": "clean_install",
                "status": "pass",
                "duration_ms": 1234,
                "detail": "installed",
                "log": [],
            },
        )

    def test_a_failure_keeps_only_the_last_forty_lines(self):
        lines = [f"line {index}" for index in range(100)]
        row = report.make("fedora-44", "upgrade", report.FAIL, 1, "broke", lines)
        self.assertEqual(len(row.row()["log"]), 40)
        self.assertEqual(row.row()["log"][0], "line 60")
        self.assertEqual(row.row()["log"][-1], "line 99")

    def test_a_negative_duration_is_clamped(self):
        row = report.make("fedora-44", "upgrade", report.PASS, -5)
        self.assertEqual(row.duration_ms, 0)

    def test_failed_statuses(self):
        self.assertTrue(report.make("d", "s", report.FAIL, 0, "x").failed)
        self.assertTrue(report.make("d", "s", report.ERROR, 0, "x").failed)
        self.assertFalse(report.make("d", "s", report.PASS, 0).failed)
        self.assertFalse(report.make("d", "s", report.SKIP, 0).failed)


class SummaryTests(unittest.TestCase):
    def test_counts_and_exit_code(self):
        rows = [
            report.make("fedora-44", "clean_install", report.PASS, 10),
            report.make("fedora-44", "upgrade", report.FAIL, 10, "broke"),
            report.make("ubuntu-2404", "clean_install", report.SKIP, 0, "no artefact"),
        ]
        summary = report.summarize(rows)
        self.assertEqual(summary["total"], 3)
        self.assertEqual(summary["pass"], 1)
        self.assertEqual(summary["fail"], 1)
        self.assertEqual(summary["skip"], 1)
        self.assertEqual(summary["exit_code"], 1)

    def test_all_pass_or_skip_exits_zero(self):
        rows = [
            report.make("fedora-44", "clean_install", report.PASS, 1),
            report.make("fedora-44", "upgrade", report.SKIP, 0, "needs clean_install"),
        ]
        self.assertEqual(report.exit_code(rows), 0)


class ValidateTests(unittest.TestCase):
    def _document(self):
        rows = [report.make("fedora-44", "clean_install", report.PASS, 10, "ok")]
        return report.build(rows, ["fedora-44"])

    def test_a_good_document_has_no_problems(self):
        self.assertEqual(report.validate(self._document()), [])

    def test_schema_version(self):
        document = self._document()
        document["schema_version"] = 99
        self.assertTrue(report.validate(document))

    def test_a_bad_status(self):
        document = self._document()
        document["scenarios"][0]["status"] = "green"
        problems = report.validate(document)
        self.assertTrue(any("status" in problem for problem in problems))

    def test_a_negative_or_float_duration(self):
        document = self._document()
        document["scenarios"][0]["duration_ms"] = -1
        self.assertTrue(report.validate(document))
        document["scenarios"][0]["duration_ms"] = 1.5
        problems = report.validate(document)
        self.assertTrue(any("duration_ms" in problem for problem in problems))

    def test_a_long_log(self):
        document = self._document()
        document["scenarios"][0]["log"] = ["x"] * 41
        problems = report.validate(document)
        self.assertTrue(any("log" in problem for problem in problems))

    def test_a_failure_without_a_detail(self):
        document = self._document()
        document["scenarios"][0]["status"] = report.FAIL
        document["scenarios"][0]["detail"] = ""
        document["summary"] = report.summarize(
            [report.make("fedora-44", "clean_install", report.FAIL, 10)]
        )
        problems = report.validate(document)
        self.assertTrue(any("without a detail" in problem for problem in problems))

    def test_a_summary_that_does_not_match_the_rows(self):
        document = self._document()
        document["summary"]["fail"] = 3
        problems = report.validate(document)
        self.assertTrue(any("counts" in problem for problem in problems))

    def test_not_an_object(self):
        self.assertTrue(report.validate(["nope"]))


class WriteTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-verify-report-")
        self.directory = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_writes_a_valid_file(self):
        rows = [report.make("fedora-44", "clean_install", report.PASS, 10, "ok")]
        path = report.write(self.directory / "nested" / "report.json", rows, ["fedora-44"])
        document = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(document["schema_version"], report.SCHEMA_VERSION)
        self.assertEqual(document["distros"], ["fedora-44"])
        self.assertEqual(report.validate(document), [])

    def test_refuses_to_write_an_invalid_report(self):
        rows = [
            report.make("fedora-44", "clean_install", report.PASS, 10),
            report.make("fedora-44", "upgrade", report.FAIL, 10, ""),
        ]
        with self.assertRaises(ValueError):
            report.write(self.directory / "report.json", rows, ["fedora-44"])
        self.assertFalse((self.directory / "report.json").exists())

    def test_the_file_is_deterministic(self):
        rows = [report.make("fedora-44", "clean_install", report.PASS, 10, "ok")]
        first = report.write(self.directory / "a.json", rows, ["fedora-44"])
        second = report.write(self.directory / "b.json", rows, ["fedora-44"])
        self.assertEqual(first.read_text(), second.read_text())


class TableTests(unittest.TestCase):
    def test_render_table_names_every_scenario(self):
        rows = [
            report.make("fedora-44", "clean_install", report.PASS, 1500),
            report.make("fedora-44", "upgrade", report.SKIP, 0, "needs clean_install"),
        ]
        table = report.render_table(rows)
        self.assertIn("clean_install", table)
        self.assertIn("upgrade", table)
        self.assertIn("1.500 s", table)
        self.assertIn("1 passed", table)
        self.assertIn("1 skipped", table)

    def test_an_empty_run_says_so(self):
        self.assertEqual(report.render_table([]), "no scenario ran")


if __name__ == "__main__":
    unittest.main()
