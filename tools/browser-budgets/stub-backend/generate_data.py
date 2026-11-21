#!/usr/bin/env python3
"""Writes stub-backend/data/*.json deterministically (WP-UI-09.T5).

The three files are committed; `tests/test_stub_contract.py` regenerates them in
memory and fails when they differ, so the data and this script cannot drift.
Every number that is a quantity or an amount is a decimal *string* (ADR-12);
the files contain no JSON float at all.

    python3 tools/browser-budgets/stub-backend/generate_data.py
"""

from __future__ import annotations

import json
from decimal import Decimal
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATA = HERE / "data"

DOCTYPE = "sales_invoice"
CHILD_DOCTYPE = "sales_invoice_item"
LIST_ROWS = 2000
CHILD_ROWS = 20
STATUSES = ("Draft", "Paid", "Overdue")
CUSTOMERS = 40


def record_id(group: int, number: int) -> str:
    """A canonical UUIDv7-shaped id, deterministic."""
    return f"0192f3c4-{group:04x}-7000-8000-{number:012x}"


def meta() -> dict:
    return {
        DOCTYPE: {
            "doctype": DOCTYPE,
            "label": "Sales Invoice",
            "version": 1,
            "is_posted": True,
            "fields": [
                {"name": "name", "label": "Number", "type": "text", "read_only": True,
                 "hints": {"in_list_view": True}},
                {"name": "customer", "label": "Customer", "type": "link", "required": True,
                 "link_to": "customer", "hints": {"in_list_view": True}},
                {"name": "posting_date", "label": "Posting date", "type": "date", "required": True,
                 "hints": {"in_list_view": True}},
                {"name": "status", "label": "Status", "type": "text", "select": list(STATUSES),
                 "default": "Draft", "hints": {"in_list_view": True}},
                {"name": "grand_total", "label": "Grand total", "type": "decimal", "read_only": True,
                 "scale": 2, "precision": 18, "default": "0.00", "hints": {"in_list_view": True}},
                {"name": "items", "label": "Items", "type": "table", "child_doctype": CHILD_DOCTYPE},
            ],
        },
        CHILD_DOCTYPE: {
            "doctype": CHILD_DOCTYPE,
            "label": "Sales Invoice Item",
            "version": 1,
            "fields": [
                {"name": "item", "label": "Item", "type": "link", "required": True, "link_to": "item"},
                {"name": "qty", "label": "Quantity", "type": "decimal", "scale": 3, "precision": 18,
                 "default": "1.000"},
                {"name": "rate", "label": "Rate", "type": "decimal", "scale": 2, "precision": 18,
                 "default": "0.00"},
                {"name": "amount", "label": "Amount", "type": "decimal", "read_only": True,
                 "scale": 2, "precision": 18, "default": "0.00"},
            ],
        },
    }


def child_rows() -> list[dict]:
    rows = []
    for idx in range(1, CHILD_ROWS + 1):
        qty = Decimal(idx % 5 + 1)
        rate = Decimal(1000 + idx * 25)
        rows.append({
            "id": record_id(2, idx),
            "idx": idx,
            "item": record_id(3, idx),
            "qty": f"{qty:.3f}",
            "rate": f"{rate:.2f}",
            "amount": f"{qty * rate:.2f}",
        })
    return rows


def total_of(rows: list[dict]) -> str:
    return f"{sum((Decimal(row['amount']) for row in rows), Decimal(0)):.2f}"


def list_rows() -> list[dict]:
    rows = []
    for number in range(1, LIST_ROWS + 1):
        day = (number - 1) % 28 + 1
        month = (number - 1) // 28 % 12 + 1
        rows.append({
            "id": record_id(1, number),
            "version": 1,
            "_state": "draft" if number % 3 else "posted",
            "name": f"INV-{number:04d}",
            "customer": record_id(4, number % CUSTOMERS + 1),
            "posting_date": f"2026-{month:02d}-{day:02d}",
            "status": STATUSES[number % len(STATUSES)],
            "grand_total": f"{Decimal(number * 125 % 100000):.2f}",
        })
    return rows


def document() -> dict:
    rows = child_rows()
    first = dict(list_rows()[0])
    first["grand_total"] = total_of(rows)
    first["items"] = rows
    return first


def render(value) -> str:
    """One compact line per file: 2,000 rows would otherwise be thousands of lines."""
    return json.dumps(value, separators=(",", ":"), sort_keys=False) + "\n"


def files() -> dict[str, str]:
    return {
        "meta.json": render(meta()),
        "list-rows.json": render(list_rows()),
        "document.json": render(document()),
    }


def main() -> int:
    DATA.mkdir(exist_ok=True)
    for name, text in files().items():
        (DATA / name).write_text(text, encoding="utf-8")
        print(f"wrote {DATA / name} ({len(text)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
