"""The `informational` flag (WP-UI-09.T6).

An `informational = true` budget's breach is reported but never fails the
exit status; a *missing* result for it still fails (absence is never a pass,
informational or not). `informational_reason` is mandatory whenever the flag
is true. The flag is carried into the result JSON so the gate/artefact show
it, even though `tools/ci/ui/perf_gate.py`'s own projection (`perf-budgets.toml`)
never carries it (see `GateProjectionTest` below) — perf_gate.py is read-only
and rejects unknown `[[budget]]` keys, so the informational mechanism lives
entirely in this package's own `aggregate`/`check` verdicts, which is what
`run.sh` (and so the CI step) actually gates on.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import _support
from _support import budgets

ENTRY = dict(_support.ENTRY)


def catalogue_with(entry_overrides, *, second=None):
    entries = [{**ENTRY, **entry_overrides}]
    if second is not None:
        entries.append({**ENTRY, "id": "test.other", **second})
    text = _support.render(entries)
    with tempfile.TemporaryDirectory() as directory:
        return budgets.load(_support.write_catalogue(directory, text))


class CatalogueValidationTest(unittest.TestCase):
    def test_informational_defaults_to_false_with_no_reason(self):
        budget = catalogue_with({}).get("test.measure")
        self.assertFalse(budget.informational)
        self.assertEqual(budget.informational_reason, "")

    def test_informational_true_with_a_reason_parses(self):
        budget = catalogue_with({
            "informational": True,
            "informational_reason": "SPK-07 draft 1.5 s; CI runner measured 1.67 s; decision pending (architecture owner)",
        }).get("test.measure")
        self.assertTrue(budget.informational)
        self.assertIn("pending (architecture owner)", budget.informational_reason)

    def test_informational_true_without_a_reason_is_rejected(self):
        with self.assertRaises(budgets.BudgetError) as raised:
            catalogue_with({"informational": True})
        self.assertIn("informational_reason", str(raised.exception))

    def test_informational_true_with_a_blank_reason_is_rejected(self):
        with self.assertRaises(budgets.BudgetError):
            catalogue_with({"informational": True, "informational_reason": "   "})

    def test_a_reason_without_the_flag_is_rejected(self):
        with self.assertRaises(budgets.BudgetError) as raised:
            catalogue_with({"informational_reason": "nobody asked for this"})
        self.assertIn("informational` is not true", str(raised.exception))

    def test_non_boolean_informational_is_rejected(self):
        with self.assertRaises(budgets.BudgetError):
            catalogue_with({"informational": "yes", "informational_reason": "x"})


class RealCatalogueInformationalTest(unittest.TestCase):
    """`browser.startup.warm` is the one budget the lead has not decided on yet."""

    def setUp(self):
        self.catalogue = budgets.load(_support.BUDGETS)

    def test_only_startup_warm_is_informational(self):
        informational = [b.id for b in self.catalogue.budgets if b.informational]
        self.assertEqual(informational, ["browser.startup.warm"])

    def test_startup_warm_reason_names_the_decision_pending(self):
        budget = self.catalogue.get("browser.startup.warm")
        self.assertIn("architecture owner", budget.informational_reason)
        self.assertIn("1.5", budget.informational_reason)
        self.assertIn("1.67", budget.informational_reason)

    def test_startup_warm_max_is_unchanged(self):
        # The informational flag must never be used to quietly also raise the max.
        self.assertEqual(self.catalogue.get("browser.startup.warm").max, 1500)

    def test_every_other_budget_is_blocking(self):
        for budget in self.catalogue.budgets:
            if budget.id == "browser.startup.warm":
                continue
            self.assertFalse(budget.informational, budget.id)
            self.assertEqual(budget.informational_reason, "", budget.id)


class AggregateInformationalTest(unittest.TestCase):
    """Rule 1: an informational breach passes; a missing informational result still fails."""

    def catalogue(self):
        return catalogue_with(
            {"informational": True, "informational_reason": "lead decision pending"},
            second={},
        )

    def test_informational_breach_does_not_fail_the_verdict(self):
        runs = [_support.raw_run({"test.measure": [500], "test.other": [50]})]
        verdict = budgets.aggregate(self.catalogue(), runs, runner="fixture", commit="local")
        self.assertTrue(verdict.ok, verdict.reason())
        breach = next(r for r in verdict.records if r["id"] == "test.measure")
        self.assertEqual(breach["status"], "BREACH")
        self.assertTrue(breach["informational"])
        self.assertEqual([r["id"] for r in verdict.informational_breaches], ["test.measure"])
        self.assertEqual(verdict.breaches, ())

    def test_missing_informational_result_still_fails(self):
        runs = [_support.raw_run({"test.other": [50]})]
        verdict = budgets.aggregate(self.catalogue(), runs, runner="fixture", commit="local")
        self.assertFalse(verdict.ok)
        missing = dict(verdict.missing)
        self.assertIn("test.measure", missing)
        self.assertEqual(verdict.informational_breaches, ())

    def test_blocking_budget_breach_still_fails_alongside_an_informational_one(self):
        runs = [_support.raw_run({"test.measure": [500], "test.other": [500]})]
        verdict = budgets.aggregate(self.catalogue(), runs, runner="fixture", commit="local")
        self.assertFalse(verdict.ok)
        self.assertEqual([r["id"] for r in verdict.breaches], ["test.other"])
        self.assertEqual([r["id"] for r in verdict.informational_breaches], ["test.measure"])

    def test_informational_record_carries_the_flag_in_json(self):
        runs = [_support.raw_run({"test.measure": [10], "test.other": [10]})]
        verdict = budgets.aggregate(self.catalogue(), runs, runner="fixture", commit="local")
        with tempfile.TemporaryDirectory() as directory:
            budgets.write_results(Path(directory), verdict)
            measure = json.loads((Path(directory) / "test.measure.json").read_text())
            other = json.loads((Path(directory) / "test.other.json").read_text())
        self.assertIs(measure["informational"], True)
        self.assertIs(other["informational"], False)


class CheckResultsInformationalTest(unittest.TestCase):
    def catalogue(self):
        return catalogue_with(
            {"informational": True, "informational_reason": "lead decision pending"},
            second={},
        )

    def write(self, directory, values):
        verdict = budgets.aggregate(self.catalogue(), [_support.raw_run(values)],
                                    runner="ci-linux", commit="local")
        budgets.write_results(Path(directory), verdict)
        return verdict

    def test_check_accepts_an_informational_breach(self):
        with tempfile.TemporaryDirectory() as directory:
            self.write(directory, {"test.measure": [500], "test.other": [50]})
            problems = budgets.check_results(Path(directory), self.catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(problems, [])

    def test_check_rejects_a_missing_informational_result(self):
        with tempfile.TemporaryDirectory() as directory:
            self.write(directory, {"test.measure": [10], "test.other": [10]})
            (Path(directory) / "test.measure.json").unlink()
            problems = budgets.check_results(Path(directory), self.catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertEqual(len(problems), 1)
        self.assertIn("no result for budget 'test.measure'", problems[0])

    def test_check_still_rejects_a_blocking_breach(self):
        with tempfile.TemporaryDirectory() as directory:
            self.write(directory, {"test.measure": [10], "test.other": [500]})
            problems = budgets.check_results(Path(directory), self.catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("breach" in problem for problem in problems))
        self.assertTrue(any("test.other" in problem for problem in problems))

    def test_check_rejects_an_informational_mismatch(self):
        with tempfile.TemporaryDirectory() as directory:
            self.write(directory, {"test.measure": [10], "test.other": [10]})
            path = Path(directory) / "test.measure.json"
            record = json.loads(path.read_text())
            record["informational"] = False
            path.write_text(json.dumps(record), encoding="utf-8")
            problems = budgets.check_results(Path(directory), self.catalogue(),
                                             runner="ci-linux", commit="local")
        self.assertTrue(any("informational is False" in problem for problem in problems))


class CliInformationalTest(unittest.TestCase):
    """The same rules through `budget_cli.py aggregate`/`check`, which is what `run.sh`
    (and so the CI step) actually runs and gates its exit status on."""

    def setUp(self):
        import sys
        if str(_support.ROOT) not in sys.path:
            sys.path.insert(0, str(_support.ROOT))
        import budget_cli
        self.budget_cli = budget_cli

    def run_cli(self, *argv):
        import contextlib
        import io
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = self.budget_cli.main(list(argv))
        return status, out.getvalue(), err.getvalue()

    def test_cli_aggregate_exits_zero_on_an_informational_breach(self):
        text = _support.render([
            {**ENTRY, "informational": True, "informational_reason": "lead decision pending"},
        ])
        with tempfile.TemporaryDirectory() as directory:
            budgets_path = _support.write_catalogue(directory, text)
            raw = _support.write_runs(Path(directory) / "raw",
                                      [_support.raw_run({"test.measure": [500]})])
            out = Path(directory) / "perf"
            status, stdout, err = self.run_cli(
                "aggregate", "--budgets", str(budgets_path), "--raw", str(raw),
                "--out", str(out), "--runs", "1", "--runner", "fixture", "--commit", "local")
        self.assertEqual(status, 0, err)
        self.assertIn("(informational)", stdout)

    def test_cli_aggregate_exits_one_when_the_informational_result_is_missing(self):
        text = _support.render([
            {**ENTRY, "informational": True, "informational_reason": "lead decision pending"},
        ])
        with tempfile.TemporaryDirectory() as directory:
            budgets_path = _support.write_catalogue(directory, text)
            raw = _support.write_runs(Path(directory) / "raw", [_support.raw_run({})])
            out = Path(directory) / "perf"
            status, _, err = self.run_cli(
                "aggregate", "--budgets", str(budgets_path), "--raw", str(raw),
                "--out", str(out), "--runs", "1", "--runner", "fixture", "--commit", "local")
        self.assertEqual(status, 1)
        self.assertIn("MISSING test.measure", err)


if __name__ == "__main__":
    unittest.main()
