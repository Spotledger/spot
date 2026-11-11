"""verify_dataset catches real corruption: tampering, totals, stock, links."""

import hashlib
import json
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.generate import generate_dataset
from datagen.profile import load_profile
from datagen.verify import VerifyError, verify_dataset


def rewrite_manifest(directory: Path) -> None:
    """Recompute digests and row counts after mutating data files."""
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    for name in list(manifest["files"]):
        data = (directory / name).read_bytes()
        manifest["files"][name] = {
            "rows": data.count(b"\n"),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
    (directory / "manifest.json").write_bytes(
        (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    )


def load_rows(path: Path):
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class VerifyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls._tmp.name) / "out"
        generate_dataset(load_profile("tiny"), 42, cls.out)
        cls._counter = 0

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    @classmethod
    def fresh_copy(cls) -> Path:
        """A pristine copy of the generated output to corrupt in one place."""
        cls._counter += 1
        copy_dir = Path(cls._tmp.name) / f"copy-{cls._counter}"
        copy_dir.mkdir()
        for source in cls.out.iterdir():
            (copy_dir / source.name).write_bytes(source.read_bytes())
        return copy_dir

    @classmethod
    def save_rows(cls, path: Path, rows) -> None:
        path.write_bytes(
            "".join(
                json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows
            ).encode("utf-8")
        )

    def assert_violation(self, target: Path, fragment: str):
        """verify_dataset must surface at least one violation containing fragment."""
        try:
            errors = verify_dataset(str(target))
        except VerifyError as exc:
            errors = [str(exc)]
        self.assertTrue(
            any(fragment in error for error in errors),
            f"{fragment!r} not reported; got: {errors}",
        )

    def test_good_output_passes(self):
        self.assertEqual(verify_dataset(str(self.out)), [])

    def test_detects_tampered_content_without_manifest_update(self):
        target = self.fresh_copy()
        payments = target / "payment.jsonl"
        rows = load_rows(payments)
        rows[0]["amount"] = "999.99"
        self.save_rows(payments, rows)
        self.assert_violation(target, "does not match manifest")

    def test_detects_row_count_mismatch(self):
        target = self.fresh_copy()
        manifest_path = target / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["files"]["companies.jsonl"]["rows"] = 999
        manifest_path.write_bytes(
            (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
        )
        self.assert_violation(target, "manifest says 999 rows")

    def test_detects_dangling_item_reference(self):
        target = self.fresh_copy()
        sales = target / "sales_invoice.jsonl"
        rows = load_rows(sales)
        rows[0]["lines"][0]["item_id"] = "IT-999999"
        self.save_rows(sales, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "unknown item 'IT-999999'")

    def test_detects_wrong_invoice_total(self):
        target = self.fresh_copy()
        sales = target / "sales_invoice.jsonl"
        rows = load_rows(sales)
        rows[0]["total"] = "1.00"
        self.save_rows(sales, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "total 1.00 != lines sum")

    def test_detects_payment_that_does_not_settle_the_invoice(self):
        target = self.fresh_copy()
        payments = target / "payment.jsonl"
        rows = load_rows(payments)
        rows[0]["amount"] = "99999999.99"
        rows[0]["allocations"][0]["amount"] = "99999999.99"
        self.save_rows(payments, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "!= invoice total")

    def test_detects_negative_on_hand_stock(self):
        target = self.fresh_copy()
        path = target / "stock_adjustment.jsonl"
        rows = load_rows(path)
        changed = False
        for index, row in enumerate(rows):
            if row["direction"] == "out":
                quantity = Decimal("9999999.000")
                rate = Decimal(row["rate"])
                rows[index]["quantity"] = f"{quantity:.3f}"
                rows[index]["amount"] = f"{(quantity * rate).quantize(Decimal('0.01')):.2f}"
                changed = True
                break
        self.assertTrue(changed, "tiny has no 'out' adjustment to corrupt")
        self.save_rows(path, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "would go negative")

    def test_detects_cross_company_transfer(self):
        target = self.fresh_copy()
        warehouses = load_rows(target / "warehouses.jsonl")
        path = target / "stock_transfer.jsonl"
        rows = load_rows(path)
        foreign = [
            warehouse["warehouse_id"]
            for warehouse in warehouses
            if warehouse["company_id"] != rows[0]["company_id"]
        ]
        self.assertTrue(foreign, "tiny must span more than one company")
        rows[0]["from_warehouse_id"] = foreign[0]
        self.save_rows(path, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "from-warehouse belongs to another company")

    def test_detects_unbalanced_journal_entry(self):
        target = self.fresh_copy()
        path = target / "journal_entry.jsonl"
        rows = load_rows(path)
        lines = rows[0]["lines"]
        lines.append(
            {
                "account_id": lines[0]["account_id"],
                "debit": "123.45",
                "credit": "0.00",
            }
        )
        self.save_rows(path, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "unbalanced")

    def test_detects_over_returned_quantity(self):
        target = self.fresh_copy()
        path = target / "sales_return.jsonl"
        rows = load_rows(path)
        self.assertTrue(rows, "tiny should generate sales returns")
        rows[0]["lines"][0]["quantity"] = "99999.000"
        rate = Decimal(rows[0]["lines"][0]["rate"])
        rows[0]["lines"][0]["amount"] = (
            Decimal("99999.000") * rate
        ).quantize(Decimal("0.01")).__format__("f")
        self.save_rows(path, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "exceeds original")

    def test_detects_opening_row_dated_inside_the_period(self):
        target = self.fresh_copy()
        path = target / "opening_invoice.jsonl"
        rows = load_rows(path)
        rows[0]["date"] = "2026-03-01"
        self.save_rows(path, rows)
        rewrite_manifest(target)
        self.assert_violation(target, "opening row dated")


if __name__ == "__main__":
    unittest.main()
