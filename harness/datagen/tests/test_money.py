"""Money and quantity arithmetic: ADR-12, no floats anywhere."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.money import (
    MONEY_SCALE,
    QUANTITY_SCALE,
    MoneyError,
    format_money,
    format_quantity,
    parse_fixed,
    round_half_even,
)


class RoundHalfEvenTests(unittest.TestCase):
    def test_halves_go_to_even(self):
        self.assertEqual(round_half_even(5, 10), 0)  # 0.5 -> 0
        self.assertEqual(round_half_even(15, 10), 2)  # 1.5 -> 2
        self.assertEqual(round_half_even(25, 10), 2)  # 2.5 -> 2
        self.assertEqual(round_half_even(35, 10), 4)  # 3.5 -> 4
        self.assertEqual(round_half_even(26, 10), 3)
        self.assertEqual(round_half_even(24, 10), 2)
        self.assertEqual(round_half_even(0, 10), 0)
        self.assertEqual(round_half_even(999, 1000), 1)

    def test_line_amount_rule(self):
        # amount = quantity (milli) x rate (paisa), half-even at 3 decimals
        self.assertEqual(round_half_even(11857 * 353431, 1000), 4190631)
        self.assertEqual(round_half_even(3650 * 62066, 1000), 226541)

    def test_rejects_bad_input(self):
        with self.assertRaises(ValueError):
            round_half_even(-1, 10)
        with self.assertRaises(ValueError):
            round_half_even(1, 0)


class FormattingTests(unittest.TestCase):
    def test_money_is_two_decimals(self):
        self.assertEqual(format_money(0), "0.00")
        self.assertEqual(format_money(5), "0.05")
        self.assertEqual(format_money(12345), "123.45")
        with self.assertRaises(ValueError):
            format_money(-1)

    def test_quantity_is_three_decimals(self):
        self.assertEqual(format_quantity(0), "0.000")
        self.assertEqual(format_quantity(650), "0.650")
        self.assertEqual(format_quantity(11857), "11.857")
        with self.assertRaises(ValueError):
            format_quantity(-1)


class ParseFixedTests(unittest.TestCase):
    def test_round_trip(self):
        for paisa in (0, 5, 99, 12345, 987654321):
            self.assertEqual(parse_fixed(format_money(paisa), MONEY_SCALE), paisa)
        for milli in (0, 1, 999, 11857):
            self.assertEqual(parse_fixed(format_quantity(milli), QUANTITY_SCALE), milli)

    def test_scale_must_match_exactly(self):
        with self.assertRaises(MoneyError):
            parse_fixed("12.3", 2)
        with self.assertRaises(MoneyError):
            parse_fixed("12.345", 2)

    def test_rejects_malformed_input(self):
        for text in ("12", "12.", "-1.00", "+1.00", "1.2.3", "", " 1.00", "1.0 ", 12.34, None):
            with self.assertRaises(MoneyError):
                parse_fixed(text, 2)

    def test_leading_zeros_are_accepted(self):
        self.assertEqual(parse_fixed("012.30", 2), 1230)


if __name__ == "__main__":
    unittest.main()