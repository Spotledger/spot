"""Campaign mode: the state machine, the summary, resumability and the settings assertion."""

from __future__ import annotations

import os
import unittest

import support  # noqa: F401  (path bootstrap)

from campaign import (
    CampaignConfig,
    append_outcome,
    cycle_seeds,
    load_outcomes,
    outcome_from_report,
    run_campaign,
)
from checks import check_all
from l7history import COMMIT_CLASSES
from storesnapshot import StoreSnapshot
from support import entry, history
from targets import FakeTarget, RecoveryInfo, SettingsError, assert_durable_settings
from workload import WorkloadConfig


def fake(seed: int, *, fsync: bool = True, sync: bool = True, out_path: str = "", operations: int = 30) -> FakeTarget:
    return FakeTarget(
        durability={"fsync": fsync, "synchronous_commit": sync, "full_page_writes": True},
        config=WorkloadConfig(seed=seed, workers=1, operations=operations, profile="smoke"),
        history_path=os.path.join(support.tmpdir(), "h.jsonl"),
        clock=lambda: 0,
    )


def config(seed: int = 7, cycles: int = 3, out_path: str = "") -> CampaignConfig:
    return CampaignConfig(
        campaign_seed=seed,
        cycles=cycles,
        operations=30,
        workers=1,
        profile="smoke",
        driver="process-kill",
        cut_min_ops=1,
        recovery_timeout_us=1_000_000,
        enforce_settings=True,
        out_path=out_path,
    )


class CampaignTests(unittest.TestCase):
    def test_durable_campaign_passes(self) -> None:
        summary = run_campaign(fake(7), config(cycles=5))
        self.assertEqual(summary.cycles_run, 5)
        self.assertEqual(summary.violations, 0)
        self.assertFalse(summary.stopped_on_violation)
        self.assertIn("5 cycles, 0 violation(s)", summary.to_text())
        for name in COMMIT_CLASSES:
            self.assertIn(name, summary.classification_totals())

    def test_unsafe_settings_stop_the_campaign_on_the_first_violation(self) -> None:
        cfg = config(cycles=10)
        cfg = CampaignConfig(**{**cfg.__dict__, "enforce_settings": False})
        summary = run_campaign(fake(7, fsync=False, sync=False), cfg)
        self.assertTrue(summary.stopped_on_violation)
        self.assertEqual(summary.violations, 1)
        self.assertEqual(summary.cycles_run, 1)

    def test_resume_skips_recorded_cycles(self) -> None:
        out_path = os.path.join(support.tmpdir(), "results.jsonl")
        first = run_campaign(fake(7), config(cycles=3, out_path=out_path))
        self.assertEqual(first.cycles_run, 3)
        again = run_campaign(fake(7), config(cycles=5, out_path=out_path))
        self.assertEqual(again.cycles_run, 5)
        self.assertEqual(len(load_outcomes(out_path)), 5)
        # The first three are unchanged: resume did not rerun them.
        self.assertEqual([o.cycle for o in load_outcomes(out_path)], [0, 1, 2, 3, 4])

    def test_recorded_outcome_round_trips(self) -> None:
        path = os.path.join(support.tmpdir(), "one.jsonl")
        report = check_all(history(entry(0, 0)), StoreSnapshot(), seed=1)
        outcome = outcome_from_report(0, 99, 5, "process-kill", RecoveryInfo(100, 50), report, {"fsync": "on"})
        append_outcome(path, outcome)
        read = load_outcomes(path)[0]
        self.assertEqual(read.seed, 99)
        self.assertEqual(read.cut_after_ops, 5)
        self.assertEqual(read.recovery_us, 100)
        self.assertEqual(read.wal_replay_us, 50)
        self.assertTrue(read.invariants_ok)

    def test_cycle_seeds_are_used_in_order(self) -> None:
        out_path = os.path.join(support.tmpdir(), "ordered.jsonl")
        run_campaign(fake(7), config(cycles=4, out_path=out_path))
        recorded = load_outcomes(out_path)
        expected = cycle_seeds(7, 4, 30, 1)
        self.assertEqual([(o.seed, o.cut_after_ops) for o in recorded], expected)


class SettingsTests(unittest.TestCase):
    def test_all_on_passes(self) -> None:
        assert_durable_settings({"fsync": "on", "synchronous_commit": "on", "full_page_writes": "on"})

    def test_any_off_fails(self) -> None:
        for bad in (
            {"fsync": "off", "synchronous_commit": "on", "full_page_writes": "on"},
            {"fsync": "on", "synchronous_commit": "off", "full_page_writes": "on"},
            {"fsync": "on", "synchronous_commit": "on", "full_page_writes": "off"},
            {},
        ):
            with self.assertRaises(SettingsError):
                assert_durable_settings(bad)

    def test_fake_target_reports_its_settings(self) -> None:
        target = fake(1)
        target.prepare(1)
        self.assertEqual(
            target.settings(),
            {"fsync": "on", "synchronous_commit": "on", "full_page_writes": "on"},
        )


if __name__ == "__main__":
    unittest.main()
