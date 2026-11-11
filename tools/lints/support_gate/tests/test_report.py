from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import report  # noqa: E402
from test_gate import make_result  # noqa: E402


class ReportTests(unittest.TestCase):
    def test_render_carries_the_gate_the_summary_and_each_incident(self) -> None:
        result = make_result()
        result["incidents"][0]["trace"] = [
            {
                "seq": 1,
                "tool": "diagnose",
                "tenant": "tenant-atlas",
                "status": "ok",
                "detail": "diagnosis observed",
                "applied": False,
                "changes": 0,
            },
            {
                "seq": 2,
                "tool": "apply_remedy",
                "tenant": "tenant-atlas",
                "status": "ok",
                "detail": "the fix ended `verified`",
                "remedy": "job.retry_failed",
                "reversible": True,
                "applied": True,
                "changes": 1,
            },
        ]
        result["incidents"][0]["audit"] = [
            {"action": "verified", "state": "verified", "detail": "checks passed"}
        ]
        text = report.render(result)
        self.assertIn("Gate: PASS", text)
        self.assertIn("Support evaluation", text)
        self.assertIn("`a`", text)
        self.assertIn("`b`", text)
        self.assertIn("job.retry_failed", text)
        self.assertIn("`diagnose`", text)
        self.assertIn("checks passed", text)

    def test_render_marks_a_blocked_result(self) -> None:
        result = make_result(harm=1)
        result["summary"]["gate_passed"] = False
        result["incidents"][0]["harm"] = ["a forbidden call"]
        text = report.render(result)
        self.assertIn("Gate: BLOCKED", text)
        self.assertIn("a forbidden call", text)

    def test_render_handles_an_empty_run(self) -> None:
        result = make_result()
        result["incidents"] = []
        result["summary"]["total"] = 0
        text = report.render(result)
        self.assertIn("Incidents | 0", text)

    def test_cli_writes_the_report(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "result.json"
            source.write_text(json.dumps(make_result()), encoding="utf-8")
            destination = root / "report.md"
            code = report.main(["--result", str(source), "--out", str(destination)])
            self.assertEqual(code, 0)
            self.assertIn("Gate: PASS", destination.read_text(encoding="utf-8"))

    def test_cli_refuses_a_malformed_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "broken.json"
            source.write_text("{not json", encoding="utf-8")
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                code = report.main(["--result", str(source)])
            self.assertEqual(code, 2)
            self.assertIn("report", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
