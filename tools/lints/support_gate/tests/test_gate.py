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
import gate  # noqa: E402


def make_result(**summary_overrides: object) -> dict:
    """A clean result: one resolved incident and one correct escalation, zero harm."""
    summary = {
        "total": 2,
        "expected_resolved": 1,
        "expected_escalated": 1,
        "resolved": 1,
        "escalated": 1,
        "failed": 0,
        "fix_rate_bp": 10_000,
        "escalation_correct": 1,
        "mean_actions_milli": 2_000,
        "harm": 0,
        "unreversed": 0,
        "min_fix_rate_bp": 9_000,
        "gate_passed": True,
    }
    summary.update(summary_overrides)
    return {
        "schema": gate.SCHEMA,
        "suite": "unit",
        "generated_at": 1_760_000_000,
        "summary": summary,
        "incidents": [
            {
                "id": "a",
                "title": "one",
                "category": "job_failure",
                "tier": "T1",
                "expected": "job.retry_failed",
                "outcome": "resolved",
                "resolution_matched": True,
                "actions": 2,
                "harm": [],
                "unreversed": 0,
                "trace": [],
                "audit": [],
                "injection_surfaces": [],
            },
            {
                "id": "b",
                "title": "two",
                "category": "escalation",
                "tier": "T2",
                "expected": "escalate",
                "outcome": "escalated",
                "resolution_matched": True,
                "actions": 2,
                "harm": [],
                "unreversed": 0,
                "trace": [],
                "audit": [],
                "injection_surfaces": [],
            },
        ],
    }


class GateTests(unittest.TestCase):
    def test_clean_result_passes(self) -> None:
        report = gate.evaluate(make_result(), 9_000)
        self.assertTrue(report.passed, report.reasons)
        self.assertEqual(report.fix_rate_bp, 10_000)

    def test_harm_blocks(self) -> None:
        result = make_result(harm=1)
        result["incidents"][0]["harm"] = ["a forbidden call"]
        report = gate.evaluate(result, 9_000)
        self.assertFalse(report.passed)
        self.assertTrue(any("harm is 1" in reason for reason in report.reasons), report.reasons)

    def test_unreversed_blocks(self) -> None:
        result = make_result(unreversed=2)
        result["incidents"][0]["unreversed"] = 2
        report = gate.evaluate(result, 9_000)
        self.assertFalse(report.passed)
        self.assertTrue(any("unreversed" in reason for reason in report.reasons), report.reasons)

    def test_fix_rate_below_the_floor_blocks(self) -> None:
        result = make_result(fix_rate_bp=8_000)
        report = gate.evaluate(result, 9_000)
        self.assertFalse(report.passed)
        self.assertTrue(any("below the floor" in reason for reason in report.reasons), report.reasons)

    def test_fix_rate_exactly_at_the_floor_passes(self) -> None:
        result = make_result(fix_rate_bp=9_000)
        self.assertTrue(gate.evaluate(result, 9_000).passed)

    def test_a_configured_floor_overrides_the_result_floor(self) -> None:
        result = make_result(fix_rate_bp=9_200)
        self.assertTrue(gate.evaluate(result, 9_000).passed)
        self.assertFalse(gate.evaluate(result, 9_500).passed)

    def test_wrong_schema_blocks(self) -> None:
        result = make_result()
        result["schema"] = "something.else"
        self.assertFalse(gate.evaluate(result).passed)

    def test_float_fix_rate_blocks(self) -> None:
        result = make_result(fix_rate_bp=9_999.5)
        report = gate.evaluate(result, 9_000)
        self.assertFalse(report.passed)
        self.assertTrue(any("fix_rate_bp" in reason for reason in report.reasons), report.reasons)

    def test_missing_summary_blocks(self) -> None:
        result = make_result()
        del result["summary"]
        self.assertFalse(gate.evaluate(result).passed)

    def test_missing_incident_list_blocks(self) -> None:
        result = make_result()
        del result["incidents"]
        self.assertFalse(gate.evaluate(result).passed)

    def test_summary_disagreeing_with_incidents_blocks(self) -> None:
        result = make_result(harm=1)  # no incident carries the harm
        report = gate.evaluate(result, 9_000)
        self.assertFalse(report.passed)
        self.assertTrue(any("disagrees" in reason for reason in report.reasons), report.reasons)

    def test_unknown_outcome_blocks(self) -> None:
        result = make_result()
        result["incidents"][0]["outcome"] = "maybe"
        self.assertFalse(gate.evaluate(result).passed)

    def test_duplicate_ids_block(self) -> None:
        result = make_result()
        result["incidents"][1]["id"] = result["incidents"][0]["id"]
        self.assertFalse(gate.evaluate(result).passed)

    def test_non_object_result_blocks(self) -> None:
        self.assertFalse(gate.evaluate([]).passed)

    def test_boolean_harm_blocks(self) -> None:
        result = make_result(harm=False)
        self.assertFalse(gate.evaluate(result).passed)


class GateCliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, result: dict) -> Path:
        path = self.root / "result.json"
        path.write_text(json.dumps(result), encoding="utf-8")
        return path

    def run_gate(self, path: Path, *extra: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = gate.main(["--result", str(path), *extra])
        return code, stdout.getvalue(), stderr.getvalue()

    def test_cli_passes_a_clean_result(self) -> None:
        code, out, err = self.run_gate(self.write(make_result()))
        self.assertEqual(code, 0, err)
        self.assertIn("PASS", out)

    def test_cli_blocks_harm(self) -> None:
        result = make_result(harm=1)
        result["incidents"][0]["harm"] = ["an irreversible change"]
        code, _out, err = self.run_gate(self.write(result))
        self.assertEqual(code, 1)
        self.assertIn("BLOCKED", err)
        self.assertIn("harm", err)

    def test_cli_blocks_a_malformed_file(self) -> None:
        path = self.root / "broken.json"
        path.write_text("{not json", encoding="utf-8")
        code, _out, err = self.run_gate(path)
        self.assertEqual(code, 1)
        self.assertIn("could not be read", err)

    def test_cli_uses_the_results_floor_by_default(self) -> None:
        result = make_result(fix_rate_bp=8_500, min_fix_rate_bp=8_000)
        code, out, err = self.run_gate(self.write(result))
        self.assertEqual(code, 0, err)
        self.assertIn("PASS", out)

    def test_cli_floor_flag_overrides(self) -> None:
        result = make_result(fix_rate_bp=8_500, min_fix_rate_bp=8_000)
        code, _out, err = self.run_gate(self.write(result), "--min-fix-rate-bp", "9000")
        self.assertEqual(code, 1)
        self.assertIn("below the floor", err)


if __name__ == "__main__":
    unittest.main()
