"""Profile distributions hold on small-trader: seasonality, skew, paid share."""

import json
import re
import sys
import tempfile
import unittest
from collections import Counter
from datetime import date
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.generate import generate_dataset
from datagen.profile import load_profile
from datagen.verify import verify_dataset


def load_rows(path: Path):
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class SmallTraderDistributionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls._tmp.name) / "out"
        cls.profile = load_profile("small-trader")
        generate_dataset(cls.profile, 42, cls.out)

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def test_row_counts_match_the_profile(self):
        stats = json.loads((self.out / "manifest.json").read_text(encoding="utf-8"))["files"]
        self.assertEqual(stats["sales_invoice.jsonl"]["rows"], 3400)
        purchase_rows = load_rows(self.out / "purchase_invoice.jsonl")
        planned = [row for row in purchase_rows if not row["is_replenishment"]]
        replenished = [row for row in purchase_rows if row["is_replenishment"]]
        self.assertEqual(len(planned), 1600)
        self.assertTrue(replenished, "stock consumption should trigger replenishment orders")
        self.assertEqual(
            stats["purchase_invoice.jsonl"]["rows"], len(planned) + len(replenished)
        )
        self.assertEqual(stats["payment.jsonl"]["rows"], 3000)
        self.assertEqual(stats["companies.jsonl"]["rows"], 3)
        self.assertEqual(stats["warehouses.jsonl"]["rows"], 6)
        self.assertEqual(stats["items.jsonl"]["rows"], 400)
        self.assertEqual(stats["customers.jsonl"]["rows"], 250)
        self.assertEqual(stats["suppliers.jsonl"]["rows"], 120)

    def test_paid_share_is_respected(self):
        rows = load_rows(self.out / "payment.jsonl")
        receivable = sum(1 for row in rows if row["direction"] == "receivable")
        payable = sum(1 for row in rows if row["direction"] == "payable")
        self.assertEqual(receivable, 3400 * 600 // 1000)
        self.assertEqual(payable, 1600 * 600 // 1000)

    def test_line_counts_stay_in_the_triangular_window(self):
        counts = []
        for path in ("sales_invoice.jsonl", "purchase_invoice.jsonl"):
            counts.extend(len(row["lines"]) for row in load_rows(self.out / path))
        self.assertTrue(all(1 <= count <= 6 for count in counts))
        self.assertLess(1.0, sum(counts) / len(counts))
        self.assertLess(sum(counts) / len(counts), 6.0)

    def test_seasonality_moves_monthly_volume(self):
        monthly = Counter()
        for row in load_rows(self.out / "sales_invoice.jsonl"):
            monthly[int(row["date"][5:7])] += 1
        # seasonality: December 14 is the peak, February 6 the trough
        self.assertGreater(monthly[12], monthly[2])

    def test_popular_items_dominate_references(self):
        references = Counter()
        for path in ("sales_invoice.jsonl", "purchase_invoice.jsonl"):
            for row in load_rows(self.out / path):
                for line in row["lines"]:
                    references[line["item_id"]] += 1
        ordered = references.most_common()
        # skew 3 puts roughly 8x the weight on rank 1 versus rank 2
        self.assertGreater(ordered[0][1], 4 * ordered[1][1])
        # only the head of the zipf tail is reachable at this dataset size
        self.assertGreater(len(ordered), 10)
        self.assertLess(len(ordered), 400)

    def test_payments_land_within_the_delay_window(self):
        invoice_dates = {}
        for row in load_rows(self.out / "sales_invoice.jsonl"):
            invoice_dates[row["invoice_id"]] = row["date"]
        for row in load_rows(self.out / "purchase_invoice.jsonl"):
            invoice_dates[row["invoice_id"]] = row["date"]
        for payment in load_rows(self.out / "payment.jsonl"):
            invoice_date = date.fromisoformat(invoice_dates[payment["allocations"][0]["invoice_id"]])
            payment_date = date.fromisoformat(payment["date"])
            self.assertGreaterEqual(payment_date, invoice_date)
            delay = (payment_date - invoice_date).days
            clamped = payment_date == date(2026, 12, 31)
            if not clamped:
                self.assertGreaterEqual(delay, 5)
            self.assertLessEqual(delay, 45)

    def test_master_data_matches_counts(self):
        customers = {row["customer_id"] for row in load_rows(self.out / "customers.jsonl")}
        self.assertEqual(len(customers), 250)
        items = load_rows(self.out / "items.jsonl")
        self.assertEqual(len({row["item_id"] for row in items}), 400)
        self.assertTrue(all(row["unit"] in ("unit", "kg", "litre", "metre", "box") for row in items))
        self.assertTrue(all(re.fullmatch(r"\d+\.\d\d", row["standard_rate"]) for row in items))


    def test_verify_finds_no_violations_in_the_full_run(self):
        self.assertEqual(verify_dataset(str(self.out)), [])

    def test_every_purchase_invoice_is_dated_inside_the_period(self):
        rows = load_rows(self.out / "purchase_invoice.jsonl")
        self.assertTrue(rows)
        for row in rows:
            self.assertLessEqual(row["date"], self.profile.period_end.isoformat())
            self.assertGreaterEqual(row["date"], self.profile.period_start.isoformat())


if __name__ == "__main__":
    unittest.main()