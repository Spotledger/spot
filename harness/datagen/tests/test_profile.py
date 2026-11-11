"""Profile loading and validation, including every shipped profile."""

import hashlib
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.profile import ProfileError, load_profile, resolve_profile_path, shipped_profiles

GOOD = """\
[period]
start = "2026-01-01"
end = "2026-03-31"

[counts]
companies = 1
warehouses_per_company = 1
items = 5
customers = 4
suppliers = 3
sales_invoices = 10
purchase_invoices = 0
payments = 3
journal_entries = 2
stock_transfers = 1
stock_adjustments = 1

[distributions]
item_popularity_skew = 1
customer_activity_skew = 1
paid_share_per_mille = 300
sales_return_per_mille = 50
purchase_return_per_mille = 50
opening_stock_coverage_per_mille = 700
opening_stock_min_milli = 50
opening_stock_max_milli = 10000
opening_receivable_per_mille = 100
opening_payable_per_mille = 200
monthly_seasonality = [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]

[distributions.lines_per_invoice]
min = 1
peak = 2
max = 3

[distributions.payment_delay]
min_days = 1
max_days = 5
"""


def load_text(text, name="case.toml"):
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / name
        path.write_text(text, encoding="utf-8")
        return load_profile(str(path))


class ShippedProfileTests(unittest.TestCase):
    def test_every_shipped_profile_parses(self):
        paths = shipped_profiles()
        self.assertGreaterEqual(len(paths), 4)
        for path in paths:
            profile = load_profile(str(path))
            self.assertEqual(profile.name, path.stem)
            self.assertEqual(profile.sha256, hashlib.sha256(path.read_bytes()).hexdigest())

    def test_tiny_loads_by_name(self):
        profile = load_profile("tiny")
        self.assertEqual(profile.name, "tiny")
        self.assertEqual(profile.sales_invoices, 60)
        self.assertEqual(profile.payments, 27)
        self.assertEqual(profile.journal_entries, 20)
        self.assertEqual(profile.stock_transfers, 30)
        self.assertEqual(profile.stock_adjustments, 40)
        self.assertEqual(profile.sales_return_per_mille, 100)
        self.assertEqual(profile.purchase_return_per_mille, 150)
        self.assertEqual(profile.opening_stock_coverage_per_mille, 700)
        self.assertEqual(profile.opening_receivable_per_mille, 100)
        self.assertEqual(profile.opening_payable_per_mille, 200)

    def test_resolve_accepts_paths_and_names(self):
        shipped = shipped_profiles()
        self.assertEqual(resolve_profile_path(str(shipped[0])), shipped[0])
        with self.assertRaises(ProfileError):
            resolve_profile_path("no-such-profile-anywhere")


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self._tmp.cleanup()

    def load(self, text, name="case.toml"):
        path = Path(self._tmp.name) / name
        path.write_text(text, encoding="utf-8")
        return load_profile(str(path))

    def expect_error(self, text):
        with self.assertRaises(ProfileError):
            self.load(text)

    def test_good_profile_loads(self):
        profile = self.load(GOOD)
        self.assertEqual(profile.companies, 1)
        self.assertEqual(profile.payments, 3)
        self.assertEqual(profile.lines_peak, 2)
        self.assertEqual(profile.period_start.isoformat(), "2026-01-01")
        self.assertEqual(profile.period_end.isoformat(), "2026-03-31")

    def test_rejects_malformed_toml(self):
        self.expect_error("[period")

    def test_missing_table(self):
        self.expect_error(GOOD.replace("[counts]", "[recounts]", 1))

    def test_missing_field(self):
        self.expect_error(GOOD.replace("companies = 1\n", "", 1))

    def test_booleans_are_rejected_as_counts(self):
        self.expect_error(GOOD.replace("companies = 1", "companies = true", 1))

    def test_period_order(self):
        self.expect_error(GOOD.replace('start = "2026-01-01"', 'start = "2026-04-01"', 1))

    def test_lines_ordering(self):
        self.expect_error(GOOD.replace("peak = 2", "peak = 0", 1))

    def test_skew_ceiling(self):
        self.expect_error(GOOD.replace("item_popularity_skew = 1", "item_popularity_skew = 9", 1))

    def test_paid_share_ceiling(self):
        self.expect_error(GOOD.replace("paid_share_per_mille = 300", "paid_share_per_mille = 1001", 1))

    def test_seasonality_shape(self):
        self.expect_error(
            GOOD.replace("= [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]", "= [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]", 1)
        )

    def test_seasonality_needs_positive_weight_somewhere(self):
        self.expect_error(
            GOOD.replace("= [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]", "= [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]", 1)
        )

    def test_seasonality_rejects_negative_weights(self):
        self.expect_error(
            GOOD.replace("= [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]", "= [-1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]", 1)
        )

    def test_payment_delay_ordering(self):
        self.expect_error(GOOD.replace("min_days = 1", "min_days = 6", 1))

    def test_payment_delay_ceiling(self):
        self.expect_error(GOOD.replace("max_days = 5", "max_days = 366", 1))

    def test_payments_must_match_the_paid_share(self):
        self.expect_error(GOOD.replace("payments = 3", "payments = 4", 1))

    def test_return_share_ceiling(self):
        self.expect_error(GOOD.replace("sales_return_per_mille = 50", "sales_return_per_mille = 1001", 1))

    def test_opening_share_ceiling(self):
        self.expect_error(
            GOOD.replace("opening_receivable_per_mille = 100", "opening_receivable_per_mille = 1001", 1)
        )


if __name__ == "__main__":
    unittest.main()