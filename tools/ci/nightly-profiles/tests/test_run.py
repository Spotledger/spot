import io
import re
import sys
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import nightly_profiles as np  # noqa: E402

NIGHTLY_YML = Path(__file__).resolve().parents[4] / ".github" / "workflows" / "nightly.yml"


class Args(unittest.TestCase):
    def test_seed_from_run_id(self):
        a = np.parse_args([], {"GITHUB_RUN_ID": "9876543210"})
        self.assertEqual(a.base_seed, 9876543210)

    def test_explicit_seed_wins(self):
        a = np.parse_args(["--seed", "7", "--run-id", "99"], {})
        self.assertEqual(a.base_seed, 7)

    def test_default_seed_without_run_id(self):
        self.assertEqual(np.parse_args([], {}).base_seed, np.DEFAULT_SEED)

    def test_bad_run_id_and_seeds_are_refused(self):
        for argv in (["--run-id", "abc"], ["--seeds", "0"], ["--seed", "-1"]):
            with self.subTest(argv=argv), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                np.parse_args(argv, {})


class Plan(unittest.TestCase):
    def test_steps(self):
        steps = np.build_steps(100, Path("o"), 5)
        names = [s.name for s in steps]
        self.assertEqual(names[:2], ["l7-engine-nightly", "l8-faults-nightly"])
        self.assertEqual(len([n for n in names if n.startswith("l8-isolation")]), 10)
        self.assertEqual(steps[0].env["SPOT_L7_PROFILE"], "nightly")
        self.assertEqual(steps[0].env["SPOT_L7_SEED"], "100")
        self.assertEqual(steps[1].env["SPOT_L8_FAULTS_PROFILE"], "nightly")
        self.assertIn("l8-isolation-list-append-104", names)

    def test_dry_run_runs_nothing(self):
        buf = io.StringIO()
        with redirect_stdout(buf):
            code = np.main(["--run-id", "5", "--dry-run", "--seeds", "1"], {})
        self.assertEqual(code, 0)
        self.assertIn("seed=5", buf.getvalue())
        self.assertIn("spot-l7-engine", buf.getvalue())


class Reports(unittest.TestCase):
    def write(self, d, name, obj):
        p = Path(d) / name
        p.write_text(json.dumps(obj), encoding="utf-8")
        return p

    def test_clean_l7_report(self):
        with tempfile.TemporaryDirectory() as d:
            p = self.write(d, "a.json", {"schema": "spot-l7-report/1", "ok": True, "results": []})
            self.assertEqual(np.report_problems(p), [])

    def test_l7_violation(self):
        with tempfile.TemporaryDirectory() as d:
            p = self.write(d, "a.json", {"ok": False, "results": [{"id": "I-1", "passed": False}, {"id": "I-2", "passed": True}]})
            problems = np.report_problems(p)
            self.assertEqual(len(problems), 1)
            self.assertIn("I-1", problems[0])
            self.assertNotIn("I-2", problems[0])

    def test_fault_violation_lists_violations(self):
        with tempfile.TemporaryDirectory() as d:
            p = self.write(d, "f.json", {"ok": False, "violations": ["lost-commit acct 3"]})
            self.assertIn("lost-commit acct 3", np.report_problems(p)[0])

    def test_matrix_report_without_ok_passes(self):
        with tempfile.TemporaryDirectory() as d:
            p = self.write(d, "m.json", {"schema": "spot-l8-matrix/1", "seed": 1, "backends": []})
            self.assertEqual(np.report_problems(p), [])

    def test_missing_and_garbled_reports_fail(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertIn("missing", np.report_problems(Path(d) / "none.json")[0])
            bad = Path(d) / "bad.json"
            bad.write_text("{nope", encoding="utf-8")
            self.assertIn("unreadable", np.report_problems(bad)[0])
            arr = self.write(d, "arr.json", [])
            self.assertIn("not a JSON object", np.report_problems(arr)[0])

    def test_scan_reports(self):
        with tempfile.TemporaryDirectory() as d:
            self.write(d, "ok.json", {"ok": True})
            self.write(d, "bad.json", {"ok": False, "violations": ["x"]})
            self.assertEqual(len(np.scan_reports(Path(d))), 1)


class NightlyWiring(unittest.TestCase):
    """nightly.yml must run the profiles job and report its result (WP-TST-08.T4)."""

    def setUp(self):
        self.text = NIGHTLY_YML.read_text(encoding="utf-8")

    def job_block(self, name):
        m = re.search(rf"^  {re.escape(name)}:\n((?:(?:    .*)?\n)+)", self.text, re.MULTILINE)
        self.assertIsNotNone(m, f"job {name} missing from nightly.yml")
        return m.group(1)

    def test_job_exists_and_runs_the_script(self):
        block = self.job_block("l7-l8-profiles")
        self.assertIn("bash tools/ci/nightly-profiles/run.sh", block)
        self.assertIn("bash tools/ci/start-postgres.sh", block)
        self.assertIn("timeout-minutes: 360", block)
        self.assertIn("target/nightly-profiles/", block)

    def test_report_needs_the_job(self):
        if not re.search(r"^  report:", self.text, re.MULTILINE):
            self.skipTest("the exported (public) nightly.yml has no report job")
        block = self.job_block("report")
        m = re.search(r"^    needs: \[(.*?)\]", block, re.MULTILINE)
        self.assertIsNotNone(m)
        needs = [n.strip() for n in m.group(1).split(",")]
        self.assertIn("l7-l8-profiles", needs)
        for existing in ("rust", "python", "dotnet"):
            self.assertIn(existing, needs)


if __name__ == "__main__":
    unittest.main()
