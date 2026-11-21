"""Catalogue parsing and validation: the seven budgets and every way to get them wrong."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import _support
from _support import BUDGET_IDS, budgets

ENTRY = dict(_support.ENTRY)
COMMENT = "# reference: test hardware, 50 Mbit/s, no throttle\n"


def load_text(text):
    with tempfile.TemporaryDirectory() as directory:
        return budgets.load(_support.write_catalogue(directory, text))


class RealCatalogueTest(unittest.TestCase):
    """The committed catalogue is the shape the brief asks for."""

    def setUp(self):
        self.catalogue = budgets.load(_support.BUDGETS)

    def test_seven_budgets_in_order_with_a_metric_unit_and_quick_flag(self):
        self.assertEqual(self.catalogue.ids(), BUDGET_IDS)
        for budget in self.catalogue.budgets:
            self.assertIn(budget.metric, budgets.METRICS)
            self.assertIn(budget.unit, budgets.UNITS)
            self.assertIs(type(budget.quick), bool)
            self.assertGreater(budget.max, 0)
            self.assertIn(budget.runner, budgets.RUNNERS)
            self.assertIn(budget.profile, budgets.PROFILES)

    def test_reference_hardware_and_bandwidth_are_stated(self):
        self.assertIn("4-core", self.catalogue.reference["hardware"])
        self.assertIn("Mbit/s", self.catalogue.reference["network"])
        self.assertEqual(self.catalogue.runners, {"reference": 1.0})

    def test_the_numbers_are_the_spikes_draft_budgets(self):
        # docs/spikes/SPK-07/REPORT.md's budget column, unchanged.
        expected = {
            "browser.startup.cold": 4000,
            "browser.startup.warm": 1500,
            "browser.form.open": 500,
            "browser.list.scroll.frame": 20,
            "browser.child-edit.key": 50,
            "browser.tabs.memory": 629145600,
        }
        for id_, value in expected.items():
            self.assertEqual(self.catalogue.get(id_).max, value, id_)

    def test_only_the_size_budget_is_proposed(self):
        proposed = [budget.id for budget in self.catalogue.budgets if budget.proposed]
        self.assertEqual(proposed, ["browser.download.compressed"])

    def test_quick_mode_covers_every_declared_budget(self):
        # perf_gate.py treats a missing result as a failure, so --quick must
        # produce a sample for every declaration it ships to the gate.
        self.assertEqual(len(self.catalogue.quick()), len(self.catalogue.budgets))

    def test_gate_projection_is_in_step(self):
        self.assertEqual(_support.GATE.read_text(encoding="utf-8"),
                         budgets.gate_text(self.catalogue))


class ValidationTest(unittest.TestCase):
    """Every rejection names the problem instead of guessing."""

    def cases(self):
        one = _support.render([ENTRY])
        two = _support.render([ENTRY, {**ENTRY, "id": "test.other"}])
        return {
            "unknown budget key": (_support.render([{**ENTRY, "workload": "100k"}]), "unknown key"),
            "missing reference comment": (_support.render([{**ENTRY, "comment": None}]),
                                          "no `# reference:` comment"),
            "empty reference comment": (_support.render([{**ENTRY, "comment": "# reference:  "}]),
                                        "no `# reference:` comment"),
            "one undocumented budget": (two.replace(COMMENT, "", 1),
                                        "no `# reference:` comment"),
            "negative maximum": (_support.render([{**ENTRY, "max": -1}]), "greater than zero"),
            "zero maximum": (_support.render([{**ENTRY, "max": 0}]), "greater than zero"),
            "string maximum": (_support.render([{**ENTRY, "max": "100"}]), "expected a number"),
            "boolean maximum": (_support.render([{**ENTRY, "max": True}]), "expected a number"),
            "unknown metric": (_support.render([{**ENTRY, "metric": "p99"}]), "unknown metric"),
            "unknown unit": (_support.render([{**ENTRY, "unit": "s"}]), "unknown unit"),
            "unknown runner": (_support.render([{**ENTRY, "runner": "my-laptop"}]), "unknown runner"),
            "unknown profile": (_support.render([{**ENTRY, "profile": "wifi"}]), "unknown profile"),
            "duplicate id": (_support.render([ENTRY, ENTRY]), "duplicate budget id"),
            "invalid id": (_support.render([{**ENTRY, "id": "browser startup"}]), "invalid id"),
            "missing quick": (one.replace("quick = true\n", ""), "missing key(s) ['quick']"),
            "non-boolean quick": (one.replace("quick = true", 'quick = "yes"'), "`quick` must be"),
            "no reference table": (_support.render([ENTRY], reference=False), "[reference]"),
            "reference without network": (one.replace('network = "test network: 50 Mbit/s, 20 ms"\n', ""),
                                          "missing `network`"),
            "unknown top-level table": (one + "\n[workload]\nrows = 100000\n", "unknown top-level"),
            "multiplier below one": (one.replace("multiplier = 1.0", "multiplier = 0.5"), "multipliers must be"),
            "reference multiplier not one": (one.replace("multiplier = 1.0", "multiplier = 3.0"),
                                             "reference multiplier must be 1"),
            "percentile of bytes": (_support.render([{**ENTRY, "metric": "p95", "unit": "bytes"}]),
                                    "not a meaningful budget"),
            "empty catalogue": (_support.render([]), "at least one"),
            "missing file": (None, "no-such-budgets.toml"),
        }

    def test_invalid_catalogues_are_rejected(self):
        for name, (text, expected) in self.cases().items():
            with self.subTest(name):
                with self.assertRaises(budgets.BudgetError) as raised:
                    if text is None:
                        budgets.load(Path(tempfile.gettempdir()) / "no-such-budgets.toml")
                    else:
                        load_text(text)
                self.assertIn(expected, str(raised.exception))


class GateProjectionTest(unittest.TestCase):
    def test_projection_carries_only_the_gate_keys(self):
        catalogue = budgets.load(_support.BUDGETS)
        text = budgets.gate_text(catalogue)
        for word in ("quick", "profile", "[reference]"):
            self.assertNotIn(word, text)
        self.assertEqual(text.count("[[budget]]"), len(catalogue.budgets))
        for budget in catalogue.budgets:
            self.assertIn(f'id = "{budget.id}"', text)

    def test_write_is_idempotent(self):
        catalogue = budgets.load(_support.BUDGETS)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "perf-budgets.toml"
            self.assertTrue(budgets.write_gate_catalogue(catalogue, path))
            self.assertFalse(budgets.write_gate_catalogue(catalogue, path))
            self.assertEqual(path.read_text(encoding="utf-8"), budgets.gate_text(catalogue))


if __name__ == "__main__":
    unittest.main()
