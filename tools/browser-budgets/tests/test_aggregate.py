"""Percentile and aggregation arithmetic: integer counts stay integers."""

from __future__ import annotations

import tempfile
import unittest

import _support
from _support import budgets

FIXTURE = _support.ROOT / "tests" / "fixtures" / "fixture-budgets.toml"


def catalogue():
    return budgets.load(FIXTURE)


class PercentileTest(unittest.TestCase):
    def test_nearest_rank_picks_a_measured_value(self):
        samples = [4, 1, 3, 2]
        self.assertEqual(budgets.percentile(samples, 0.5), 2)
        self.assertEqual(budgets.percentile(samples, 0.95), 4)
        self.assertEqual(budgets.percentile(samples, 1.0), 4)

    def test_single_sample_is_its_own_p50_and_p95(self):
        self.assertEqual(budgets.percentile([7], 0.5), 7)
        self.assertEqual(budgets.percentile([7], 0.95), 7)

    def test_percentiles_keep_integers_integral(self):
        samples = list(range(1, 21))
        for p in (0.5, 0.95):
            value = budgets.percentile(samples, p)
            self.assertIs(type(value), int, f"p{p} returned {value!r}")

    def test_p95_of_twenty_samples_is_the_twentieth(self):
        samples = list(range(1, 21))
        self.assertEqual(budgets.percentile(samples, 0.95), 19)
        self.assertEqual(budgets.percentile(samples, 1.0), 20)

    def test_float_samples_are_allowed_for_timings(self):
        self.assertEqual(budgets.percentile([16.6, 16.7, 33.2], 0.5), 16.7)

    def test_empty_and_out_of_range_are_rejected(self):
        with self.assertRaises(budgets.BudgetError):
            budgets.percentile([], 0.5)
        with self.assertRaises(budgets.BudgetError):
            budgets.percentile([1, 2], 0)


class MeasureValueTest(unittest.TestCase):
    def test_max_of_bytes_is_an_int(self):
        value = budgets.measure_value([4_000_000, 4_100_000], "max")
        self.assertEqual(value, 4_100_000)
        self.assertIs(type(value), int)

    def test_p50_and_p95_of_integer_counts_stay_integers(self):
        samples = [10, 100, 1000, 10000]
        self.assertEqual(budgets.measure_value(samples, "p50"), 100)
        self.assertEqual(budgets.measure_value(samples, "p95"), 10000)
        self.assertIs(type(budgets.measure_value(samples, "p95")), int)

    def test_mean_uses_integer_division_for_counts(self):
        value = budgets.measure_value([1, 2], "mean")
        self.assertEqual(value, 1)
        self.assertIs(type(value), int)

    def test_mean_of_timings_may_be_a_float(self):
        self.assertAlmostEqual(budgets.measure_value([16.6, 16.9], "mean"), 16.75)

    def test_negative_nan_infinite_and_boolean_samples_are_rejected(self):
        for samples in ([-1], [float("nan")], [float("inf")], [True]):
            with self.subTest(samples=samples):
                with self.assertRaises(budgets.BudgetError):
                    budgets.measure_value(samples, "max")

    def test_empty_samples_are_rejected(self):
        with self.assertRaises(budgets.BudgetError):
            budgets.measure_value([], "p95")

    def test_unknown_metric_is_rejected(self):
        with self.assertRaises(budgets.BudgetError):
            budgets.measure_value([1], "p99")


class AggregateTest(unittest.TestCase):
    def test_aggregate_pools_samples_across_runs(self):
        runs = [_support.raw_run({"browser.startup.cold": [100, 200]}, run=1),
                _support.raw_run({"browser.startup.cold": [300]}, run=2)]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        record = next(r for r in verdict.records if r["id"] == "browser.startup.cold")
        self.assertEqual(record["count"], 3)
        self.assertEqual(record["samples"], [100, 200, 300])
        self.assertEqual(record["measured"], 300)  # p95 nearest rank over three

    def test_p50_and_p95_differ_on_a_wide_range(self):
        samples = [10, 10, 10, 10, 10, 10, 10, 10, 10, 100]
        cold = {"browser.startup.cold": samples}
        verdict = budgets.aggregate(catalogue(), [_support.raw_run(cold)],
                                    runner="fixture", commit="local")
        record = next(r for r in verdict.records if r["id"] == "browser.startup.cold")
        self.assertEqual(record["measured"], 100)
        self.assertEqual(record["stats"]["p50"], 10)
        self.assertEqual(record["stats"]["p95"], 100)

    def test_missing_result_is_a_failure_and_never_zero(self):
        runs = [_support.raw_run({"browser.startup.cold": [100]})]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        self.assertFalse(verdict.ok)
        missing = dict(verdict.missing)
        self.assertIn("browser.form.open", missing)
        self.assertNotIn("browser.startup.cold", missing)
        self.assertNotIn("0", [str(record["measured"]) for record in verdict.records
                               if record["id"] == "browser.form.open"])

    def test_entirely_empty_measures_lists_every_budget_as_missing(self):
        """The exact shape of a run whose browser crashed on launch (WP-UI-09.T11 evidence:
        `launchPersistentContext` closing before any sample is taken writes `measures: {}`,
        `errors: {}` and a top-level `error`). Absence is never a pass: every one of the seven
        budgets is listed as missing, with a reason, never silently skipped."""
        runs = [_support.raw_run({}, errors={})]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        self.assertFalse(verdict.ok)
        self.assertEqual(verdict.records, ())
        missing = dict(verdict.missing)
        self.assertEqual(set(missing), set(catalogue().ids()))
        for id_, why in verdict.missing:
            self.assertTrue(why, f"{id_} has no reason")
        reason = verdict.reason()
        for id_ in catalogue().ids():
            self.assertIn(f"missing {id_}", reason)

    def test_a_driver_error_is_carried_as_the_missing_reason(self):
        runs = [_support.raw_run({"browser.startup.cold": [100]},
                                 errors={"browser.form.open": "openForm returned false"})]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        missing = dict(verdict.missing)
        self.assertEqual(missing["browser.form.open"], "openForm returned false")

    def test_breach_is_reported_with_its_numbers(self):
        runs = [_support.raw_run({"browser.list.scroll.frame": [16.0, 16.0, 6000.0]})]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        self.assertFalse(verdict.ok)
        breach = next(record for record in verdict.breaches
                      if record["id"] == "browser.list.scroll.frame")
        self.assertEqual(breach["max"], 5000)
        self.assertEqual(breach["measured"], 6000.0)
        self.assertEqual(breach["status"], "BREACH")
        self.assertIn("breach browser.list.scroll.frame", verdict.reason())

    def test_equality_with_the_maximum_passes(self):
        runs = [_support.raw_run(_support.all_values(**{"browser.list.scroll.frame": [16.0, 5000]}))]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        record = next(r for r in verdict.records if r["id"] == "browser.list.scroll.frame")
        self.assertEqual(record["measured"], 5000)
        self.assertEqual(record["status"], "PASS")
        self.assertTrue(verdict.ok)

    def test_every_record_carries_the_runner_and_commit(self):
        runs = [_support.raw_run(_support.all_values())]
        verdict = budgets.aggregate(catalogue(), runs, runner="ci-linux", commit="deadbeef")
        self.assertTrue(verdict.ok, verdict.reason())
        for record in verdict.records:
            self.assertEqual(record["runner"], "ci-linux")
            self.assertEqual(record["commit"], "deadbeef")
            self.assertGreater(record["count"], 0)

    def test_fixture_extra_marks_the_evidence(self):
        runs = [_support.raw_run(_support.all_values())]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local",
                                    extra={"fixture": True})
        self.assertTrue(all(record["fixture"] is True for record in verdict.records))

    def test_unknown_ids_in_raw_evidence_are_ignored_not_counted(self):
        runs = [_support.raw_run({"browser.unknown.measure": [1, 2, 3]})]
        verdict = budgets.aggregate(catalogue(), runs, runner="fixture", commit="local")
        self.assertNotIn("browser.unknown.measure", [r["id"] for r in verdict.records])

    def test_read_runs_rejects_an_empty_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(budgets.BudgetError):
                budgets.read_runs(directory)

    def test_read_runs_reads_in_run_order(self):
        with tempfile.TemporaryDirectory() as directory:
            _support.write_runs(directory, [_support.raw_run({"browser.startup.cold": [1]}, run=1),
                                            _support.raw_run({"browser.startup.cold": [2]}, run=2)])
            runs = budgets.read_runs(directory)
        self.assertEqual([run["run"] for run in runs], [1, 2])


if __name__ == "__main__":
    unittest.main()
