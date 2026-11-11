"""Unit tests for the sample-app contract cases (WP-STR-07.T1). No containers needed."""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import contract  # noqa: E402

HERE = Path(__file__).resolve().parent.parent
REAL_CASES = HERE / "cases.json"


def write(cases) -> Path:
    """Writes cases JSON to a temporary file and returns its path."""
    handle = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(cases, handle)
    handle.close()
    return Path(handle.name)


def good_case(**overrides):
    """A valid case, with overrides."""
    case = {
        "name": "a",
        "event": '{"op":"step","name":"sample:check_hours","hours":"2.5"}',
        "expect": "ok",
    }
    case.update(overrides)
    return case


class RealCasesTests(unittest.TestCase):
    def test_the_real_cases_load(self):
        cases = contract.load_cases(REAL_CASES)
        self.assertGreaterEqual(len(cases), 6)

    def test_the_real_cases_cover_both_outcomes(self):
        cases = contract.load_cases(REAL_CASES)
        self.assertIn("ok", {case["expect"] for case in cases})
        self.assertIn("err", {case["expect"] for case in cases})

    def test_names_are_unique(self):
        names = contract.case_names(contract.load_cases(REAL_CASES))
        self.assertEqual(len(names), len(set(names)))

    def test_every_event_is_an_object(self):
        for case in contract.load_cases(REAL_CASES):
            self.assertIsInstance(json.loads(case["event"]), dict)


class ValidationTests(unittest.TestCase):
    def test_accepts_a_valid_case(self):
        cases = contract.load_cases(write([good_case()]))
        self.assertEqual(len(cases), 1)

    def test_rejects_an_empty_list(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([]))

    def test_rejects_a_missing_name(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(name="")]))

    def test_rejects_a_duplicate_name(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(), good_case()]))

    def test_rejects_an_unknown_expectation(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(expect="maybe")]))

    def test_rejects_an_err_case_without_a_code(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(expect="err")]))

    def test_rejects_an_event_that_is_not_json(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(event="{not json")]))

    def test_rejects_an_event_that_is_not_an_object(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write([good_case(event="[1,2]")]))

    def test_rejects_a_non_list(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(write({"name": "a"}))

    def test_rejects_missing_file(self):
        with self.assertRaises(contract.CasesError):
            contract.load_cases(Path("/nonexistent/cases.json"))


if __name__ == "__main__":
    unittest.main()
