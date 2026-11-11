"""Result-file schema, breach and missing-result detection, and the perf_gate contract.

The last class is the important one: it runs the real `tools/ci/ui/perf_gate.py`
against files this runner wrote, so a schema drift fails here instead of in CI.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

import _support
from _support import budgets

GATE_SCRIPT = _support.ROOT.parent / "ci" / "ui" / "perf_gate.py"

#: Passing samples for the committed catalogue (see docs/spikes/SPK-07/REPORT.md).
PASSING = {
    "browser.download.compressed": [9_800_000],
    "browser.startup.cold": [3600.0],
    "browser.startup.warm": [1400.0],
    "browser.form.open": [60.0, 75.0, 80.0],
    "browser.list.scroll.frame": [16.6, 16.7, 16.8],
    "browser.child-edit.key": [45.0, 48.0],
    "browser.tabs.memory": [344_000_000],
}


def real_catalogue():
    return budgets.load(_support.BUDGETS)


def load_perf_gate():
    spec = importlib.util.spec_from_file_location("perf_gate_under_test", GATE_SCRIPT)
    assert spec is not None and spec.loader is not None, GATE_SCRIPT
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def aggregate_into(directory, values, *, runner="ci-linux", commit="local"):
    verdict = budgets.aggregate(real_catalogue(), [_support.raw_run(values)],
                                runner=runner, commit=commit)
    budgets.write_results(Path(directory), verdict)
    return verdict


class ResultFileTest(unittest.TestCase):
    def test_written_result_has_the_gate_fields_and_our_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            path = Path(directory) / "browser.startup.cold.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            self.assertTrue(Path(directory, "browser.tabs.memory.json").exists())
        for key in ("id", "measured", "max", "runner", "commit", "metric", "unit"):
            self.assertIn(key, record)
        self.assertEqual(record["id"], "browser.startup.cold")
        self.assertEqual(record["metric"], "p95")
        self.assertEqual(record["unit"], "ms")
        self.assertEqual(record["max"], 4000)
        self.assertEqual(record["runner"], "ci-linux")
        self.assertEqual(record["commit"], "local")
        self.assertEqual(record["status"], "PASS")
        self.assertEqual(record["count"], 1)
        self.assertEqual(record["stats"]["p95"], 3600)

    def test_one_file_per_budget_and_nothing_else(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            names = sorted(path.name for path in Path(directory).iterdir())
        self.assertEqual(names, sorted(f"{id_}.json" for id_ in _support.BUDGET_IDS))

    def test_check_accepts_a_clean_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(problems, [])

    def test_check_reports_every_missing_result(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            missing = Path(directory) / "browser.form.open.json"
            missing.unlink()
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(len(problems), 1)
        self.assertIn("no result for budget 'browser.form.open'", problems[0])

    def test_check_reports_every_name_when_the_directory_is_empty(self):
        """WP-UI-09.T11: a run whose `measures` was entirely empty (the crashed-launch shape)
        writes no result file at all. The existing-but-empty directory is still a hard error
        listing every one of the seven missing names, never a skipped pass."""
        with tempfile.TemporaryDirectory() as directory:
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(len(problems), len(real_catalogue().ids()))
        for budget_id in real_catalogue().ids():
            self.assertTrue(any(f"no result for budget '{budget_id}'" in problem
                                for problem in problems), budget_id)

    def test_check_rejects_an_unknown_id(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            (Path(directory) / "browser.guess.json").write_text(
                json.dumps({"id": "browser.guess", "measured": 1, "max": 1,
                            "runner": "ci-linux", "commit": "local",
                            "metric": "max", "unit": "ms"}), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("unknown budget id" in problem for problem in problems))

    def test_check_rejects_a_mismatched_declaration(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            path = Path(directory) / "browser.startup.cold.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            record["max"] = 9000
            record["unit"] = "bytes"
            record["runner"] = "reference"
            record["commit"] = "someone-elses-sha"
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        joined = "\n".join(problems)
        self.assertIn("max is 9000", joined)
        self.assertIn("unit is 'bytes'", joined)
        self.assertIn("runner is 'reference'", joined)
        self.assertIn("commit is 'someone-elses-sha'", joined)

    def test_check_rejects_a_missing_field_and_a_non_numeric_value(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            path = Path(directory) / "browser.startup.cold.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            del record["metric"]
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
            self.assertTrue(any("missing result fields" in problem for problem in problems))

            record = json.loads(path.read_text(encoding="utf-8"))
            record["metric"] = "p95"
            record["measured"] = "3600"
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("measured is not a number" in problem for problem in problems))

    def test_check_rejects_a_negative_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            path = Path(directory) / "browser.startup.cold.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            record["measured"] = -5
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("finite and nonnegative" in problem for problem in problems))

    def test_check_rejects_a_forged_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            verdict = budgets.aggregate(real_catalogue(),
                                        [_support.raw_run({**PASSING,
                                                           "browser.child-edit.key": [62.9]})],
                                        runner="ci-linux", commit="local")
            budgets.write_results(Path(directory), verdict)
            path = Path(directory) / "browser.child-edit.key.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            record["status"] = "PASS"
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(len(problems), 1)
        self.assertIn("breach", problems[0])

    def test_check_rejects_duplicate_json_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            path = Path(directory) / "browser.startup.cold.json"
            text = path.read_text(encoding="utf-8").replace(
                '"measured": 3600.0,', '"measured": 3600.0, "measured": 1.0,', 1)
            path.write_text(text, encoding="utf-8")
            problems = budgets.check_results(Path(directory), real_catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("duplicate JSON key" in problem for problem in problems))

    def test_check_rejects_a_missing_directory(self):
        problems = budgets.check_results(Path(tempfile.gettempdir()) / "no-such-results",
                                         real_catalogue(), runner="ci-linux", commit="local")
        self.assertEqual(len(problems), 1)
        self.assertIn("result directory missing", problems[0])


class GateContractTest(unittest.TestCase):
    """The real gate must accept what we write, and reject what we must reject."""

    def setUp(self):
        self.gate = load_perf_gate()

    def test_gate_loads_the_projection(self):
        declarations = self.gate.load_budgets([_support.GATE])
        self.assertEqual(sorted(declarations), sorted(_support.BUDGET_IDS))
        self.assertEqual(declarations["browser.tabs.memory"].unit, "bytes")
        self.assertEqual(declarations["browser.child-edit.key"].max, 50)

    def test_gate_passes_our_result_files(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            outcome = self.gate.evaluate([_support.GATE], directory, "ci-linux", "hosted", "local")
        self.assertTrue(outcome.ok, outcome.markdown)
        self.assertIn("PASS", outcome.markdown)
        self.assertNotIn("MISSING", outcome.markdown)

    def test_gate_fails_on_a_missing_result(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, PASSING)
            (Path(directory) / "browser.startup.warm.json").unlink()
            outcome = self.gate.evaluate([_support.GATE], directory, "ci-linux", "hosted", "local")
        self.assertFalse(outcome.ok)
        self.assertIn("MISSING", outcome.markdown)

    def test_gate_fails_on_a_breach_for_a_hosted_runner(self):
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, {**PASSING, "browser.child-edit.key": [62.9]})
            outcome = self.gate.evaluate([_support.GATE], directory, "ci-linux", "hosted", "local")
        self.assertFalse(outcome.ok)
        self.assertIn("FAIL", outcome.markdown)

    def test_gate_warns_on_a_breach_for_a_self_hosted_runner(self):
        # The documented 3x default for an unlabelled self-hosted runner: a
        # breach is a WARNING until the lead publishes reference hardware.
        with tempfile.TemporaryDirectory() as directory:
            aggregate_into(directory, {**PASSING, "browser.child-edit.key": [62.9]})
            outcome = self.gate.evaluate([_support.GATE], directory, "ci-linux",
                                          "self-hosted", "local")
        self.assertTrue(outcome.ok, outcome.markdown)
        self.assertIn("WARNING", outcome.markdown)


if __name__ == "__main__":
    unittest.main()
