"""The command line `run.sh` drives: exit statuses, plan contents, verdicts."""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

import _support
from _support import budgets

sys.path.insert(0, str(_support.ROOT))
import budget_cli  # noqa: E402  (needs the path fix above)

FIXTURE_BUDGETS = _support.ROOT / "tests" / "fixtures" / "fixture-budgets.toml"


def run(*argv):
    """Call the CLI in-process and return (exit status, stdout, stderr)."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        status = budget_cli.main(list(argv))
    return status, out.getvalue(), err.getvalue()


def breach_catalogue(directory):
    """The fixture maxima knocked down, exactly as `run.sh --self-test` does it."""
    lines = [line if not line.startswith("max = ") else "max = 0.001"
             for line in FIXTURE_BUDGETS.read_text(encoding="utf-8").splitlines()]
    path = Path(directory) / "breach-budgets.toml"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def fixture_run(directory, values=None):
    raw = _support.write_runs(Path(directory) / "raw", [_support.raw_run(values or _support.all_values())])
    return raw, Path(directory) / "perf"


class CatalogueCommandsTest(unittest.TestCase):
    def test_validate_prints_the_catalogue(self):
        status, out, _ = run("validate", "--budgets", str(_support.BUDGETS))
        self.assertEqual(status, 0)
        for word in ("7 budgets", "browser.child-edit.key", "1 proposed"):
            self.assertIn(word, out)

    def test_validate_rejects_a_broken_catalogue_with_status_two(self):
        with tempfile.TemporaryDirectory() as directory:
            status, _, err = run("validate", "--budgets",
                                 str(_support.write_catalogue(directory, _support.render(
                                     [{**_support.ENTRY, "max": -3}]))))
        self.assertEqual(status, 2)
        self.assertIn("greater than zero", err)

    def test_gate_checks_the_committed_projection_and_detects_drift(self):
        status, _, err = run("gate", "--budgets", str(_support.BUDGETS), "--out", str(_support.GATE))
        self.assertEqual(status, 0, err)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "perf-budgets.toml"
            path.write_text('[[budget]]\nid = "stale"\n', encoding="utf-8")
            status, _, err = run("gate", "--budgets", str(_support.BUDGETS), "--out", str(path))
            self.assertEqual(status, 1)
            self.assertIn("differs", err)
            status, out, _ = run("gate", "--budgets", str(_support.BUDGETS), "--out", str(path),
                                 "--write")
            self.assertEqual(status, 0)
            self.assertIn("written", out)
            self.assertEqual(path.read_text(encoding="utf-8"),
                             budgets.gate_text(budgets.load(_support.BUDGETS)))


class PlanTest(unittest.TestCase):
    def test_quick_plan_holds_one_entry_per_quick_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "plan.json"
            status, _, err = run("plan", "--budgets", str(_support.BUDGETS), "--mode", "quick",
                                 "--runs", "1", "--out", str(path))
            self.assertEqual(status, 0, err)
            plan = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual((plan["mode"], plan["runs"]), ("quick", 1))
        self.assertEqual([entry["id"] for entry in plan["budgets"]], list(_support.BUDGET_IDS))
        for entry in plan["budgets"]:
            self.assertIn(entry["profile"], budgets.PROFILES)
            self.assertGreater(entry["max"], 0)

    def test_full_plan_asks_for_ten_runs(self):
        status, out, _ = run("plan", "--budgets", str(_support.BUDGETS), "--mode", "full",
                             "--runs", "10")
        self.assertEqual(status, 0)
        self.assertEqual(json.loads(out)["runs"], 10)


class AggregateTest(unittest.TestCase):
    def test_aggregate_writes_the_results_and_check_accepts_them(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            status, stdout, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS),
                                      "--raw", str(raw), "--out", str(out), "--runs", "1",
                                      "--runner", "fixture", "--commit", "local")
            self.assertEqual(status, 0, err)
            self.assertIn("7 result file(s)", stdout)
            self.assertEqual(len(list(out.glob("*.json"))), 7)
            status, _, err = run("check", "--budgets", str(FIXTURE_BUDGETS), "--results", str(out),
                                 "--runner", "fixture", "--commit", "local")
            self.assertEqual(status, 0, err)

    def test_aggregate_exits_one_on_a_breach(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            status, _, err = run("aggregate", "--budgets", str(breach_catalogue(directory)),
                                 "--raw", str(raw), "--out", str(out), "--runs", "1",
                                 "--runner", "fixture", "--commit", "local")
        self.assertEqual(status, 1)
        self.assertIn("breach", err)
        self.assertIn("browser.child-edit.key", err)

    def test_aggregate_exits_one_when_a_measure_is_missing(self):
        values = _support.all_values()
        values.pop("browser.tabs.memory")
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory, values)
            status, _, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw),
                                 "--out", str(out), "--runs", "1", "--runner", "fixture",
                                 "--commit", "local")
        self.assertEqual(status, 1)
        self.assertIn("MISSING browser.tabs.memory", err)

    def test_aggregate_exits_one_and_lists_every_name_when_measures_are_entirely_empty(self):
        """WP-UI-09.T11: the raw run a crashed `launchPersistentContext` writes has `measures: {}`.
        That is a hard error with every one of the seven names listed, never a skipped pass."""
        with tempfile.TemporaryDirectory() as directory:
            raw = _support.write_runs(Path(directory) / "raw", [_support.raw_run({})])
            out = Path(directory) / "perf"
            status, _, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw),
                                 "--out", str(out), "--runs", "1", "--runner", "fixture",
                                 "--commit", "local")
        self.assertEqual(status, 1)
        self.assertEqual(len(list(out.glob("*.json"))) if out.exists() else 0, 0)
        for budget_id in _support.BUDGET_IDS:
            self.assertIn(f"MISSING {budget_id}", err)

    def test_aggregate_rejects_a_short_run_count(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            status, _, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw),
                                 "--out", str(out), "--runs", "10", "--runner", "fixture",
                                 "--commit", "local")
        self.assertEqual(status, 1)
        self.assertIn("expected 10 raw run(s), found 1", err)

    def test_aggregate_marks_fixture_evidence_and_fails_without_raw_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            status, _, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw",
                                 str(Path(directory) / "nowhere"), "--out", str(out),
                                 "--runner", "fixture", "--commit", "local")
            self.assertEqual(status, 1)
            self.assertIn("raw directory missing", err)
            status, _, err = run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw),
                                 "--out", str(out), "--runner", "fixture", "--commit", "local")
            self.assertEqual(status, 0, err)
            record = json.loads((out / "browser.startup.cold.json").read_text(encoding="utf-8"))
        self.assertTrue(record["fixture"])


class CheckTest(unittest.TestCase):
    def test_check_exits_one_on_a_missing_result(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw), "--out", str(out),
                "--runner", "fixture", "--commit", "local")
            (out / "browser.form.open.json").unlink()
            status, _, err = run("check", "--budgets", str(FIXTURE_BUDGETS), "--results", str(out),
                                 "--runner", "fixture", "--commit", "local")
        self.assertEqual(status, 1)
        self.assertIn("no result for budget 'browser.form.open'", err)

    def test_check_exits_one_on_a_foreign_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, out = fixture_run(directory)
            run("aggregate", "--budgets", str(FIXTURE_BUDGETS), "--raw", str(raw), "--out", str(out),
                "--runner", "fixture", "--commit", "local")
            status, _, err = run("check", "--budgets", str(FIXTURE_BUDGETS), "--results", str(out),
                                 "--runner", "fixture", "--commit", "other")
        self.assertEqual(status, 1)
        self.assertIn("commit is 'local', expected 'other'", err)


if __name__ == "__main__":
    unittest.main()
