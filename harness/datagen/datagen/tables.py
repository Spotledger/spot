"""Table schemas: one definition of every output table.

The JSONL writer, the RFC 4180 CSV writer, the verifier and the tests all
read the same schemas from here. ``lines_field`` names the nested list on a
JSONL parent row; in CSV output those children become tables of their own,
keyed by the parent's id and a 1-based ``line`` number, so JSONL and CSV of
one seed hold exactly the same rows. Column kinds: "str" is text (ids, ISO
dates and the fixed-scale decimal strings), "bool" and "int" are typed so
CSV rows parse back to the JSONL shapes.
"""

STOCK_ORDER = (
    # Per-date processing rank of the stock-affecting tables, used by every
    # writer on a day and replayed by the verifier in the same order.
    "opening_stock",
    "purchase_invoice",
    "stock_transfer",
    "stock_adjustment",
    "sales_invoice",
    "purchase_return",
    "sales_return",
)


def _table(columns, child_name=None, child_columns=None, lines_field=None):
    return {
        "columns": tuple(columns),
        "lines_field": lines_field,
        "child": None if child_name is None else {
            "name": child_name,
            "columns": tuple(child_columns),
        },
    }


MASTER = {
    "companies": _table((
        ("company_id", "str"), ("name", "str"), ("currency", "str"),
    )),
    "warehouses": _table((
        ("warehouse_id", "str"), ("company_id", "str"), ("name", "str"),
    )),
    "items": _table((
        ("item_id", "str"), ("code", "str"), ("name", "str"),
        ("unit", "str"), ("standard_rate", "str"),
    )),
    "customers": _table((("customer_id", "str"), ("name", "str"))),
    "suppliers": _table((("supplier_id", "str"), ("name", "str"))),
    "accounts": _table((
        ("account_id", "str"), ("code", "str"), ("name", "str"), ("kind", "str"),
    )),
}

LEDGER = {
    "opening_stock": _table((
        ("opening_stock_id", "str"), ("company_id", "str"),
        ("warehouse_id", "str"), ("item_id", "str"), ("date", "str"),
        ("quantity", "str"), ("valuation_rate", "str"), ("amount", "str"),
    )),
    "opening_invoice": _table((
        ("invoice_id", "str"), ("company_id", "str"), ("direction", "str"),
        ("party_type", "str"), ("party_id", "str"), ("date", "str"),
        ("is_opening", "bool"), ("total", "str"),
    )),
    "opening_balance": _table((
        ("opening_balance_id", "str"), ("company_id", "str"),
        ("account_id", "str"), ("date", "str"), ("amount", "str"),
    )),
    "journal_entry": _table(
        (
            ("entry_id", "str"), ("company_id", "str"), ("date", "str"),
            ("memo", "str"),
        ),
        child_name="journal_entry_line",
        child_columns=(("account_id", "str"), ("debit", "str"), ("credit", "str")),
        lines_field="lines",
    ),
    "stock_transfer": _table((
        ("transfer_id", "str"), ("company_id", "str"), ("date", "str"),
        ("from_warehouse_id", "str"), ("to_warehouse_id", "str"),
        ("item_id", "str"), ("quantity", "str"), ("rate", "str"),
        ("amount", "str"),
    )),
    "stock_adjustment": _table((
        ("adjustment_id", "str"), ("company_id", "str"),
        ("warehouse_id", "str"), ("item_id", "str"), ("date", "str"),
        ("direction", "str"), ("reason_code", "str"), ("quantity", "str"),
        ("rate", "str"), ("amount", "str"),
    )),
    "sales_invoice": _table(
        (
            ("invoice_id", "str"), ("company_id", "str"),
            ("warehouse_id", "str"), ("customer_id", "str"), ("date", "str"),
            ("total", "str"),
        ),
        child_name="sales_invoice_line",
        child_columns=(
            ("item_id", "str"), ("quantity", "str"), ("rate", "str"),
            ("amount", "str"),
        ),
        lines_field="lines",
    ),
    "purchase_invoice": _table(
        (
            ("invoice_id", "str"), ("company_id", "str"),
            ("warehouse_id", "str"), ("supplier_id", "str"), ("date", "str"),
            ("total", "str"), ("is_replenishment", "bool"),
        ),
        child_name="purchase_invoice_line",
        child_columns=(
            ("item_id", "str"), ("quantity", "str"), ("rate", "str"),
            ("amount", "str"),
        ),
        lines_field="lines",
    ),
    "payment": _table(
        (
            ("payment_id", "str"), ("direction", "str"), ("party_type", "str"),
            ("party_id", "str"), ("date", "str"), ("amount", "str"),
        ),
        child_name="payment_allocation",
        child_columns=(("invoice_id", "str"), ("amount", "str")),
        lines_field="allocations",
    ),
    "sales_return": _table(
        (
            ("sales_return_id", "str"), ("company_id", "str"),
            ("warehouse_id", "str"), ("customer_id", "str"),
            ("original_invoice_id", "str"), ("date", "str"), ("total", "str"),
        ),
        child_name="sales_return_line",
        child_columns=(
            ("item_id", "str"), ("quantity", "str"), ("rate", "str"),
            ("amount", "str"), ("original_line", "int"),
        ),
        lines_field="lines",
    ),
    "purchase_return": _table(
        (
            ("purchase_return_id", "str"), ("company_id", "str"),
            ("warehouse_id", "str"), ("supplier_id", "str"),
            ("original_invoice_id", "str"), ("date", "str"), ("total", "str"),
        ),
        child_name="purchase_return_line",
        child_columns=(
            ("item_id", "str"), ("quantity", "str"), ("rate", "str"),
            ("amount", "str"), ("original_line", "int"),
        ),
        lines_field="lines",
    ),
}

TABLES = {**MASTER, **LEDGER}
MASTER_NAMES = tuple(MASTER)


def file_name(stem: str, fmt: str) -> str:
    """The output file name of one parent table in the given format."""
    return f"{stem}.{'jsonl' if fmt == 'jsonl' else 'csv'}"


def child_key(table_name: str) -> str:
    """The parent-key column name of a child table."""
    return f"{table_name}_id"


def full_child_columns(table_name: str):
    """A child table's CSV columns: parent key, 1-based line, line fields."""
    child = TABLES[table_name]["child"]
    return ((child_key(table_name), "str"), ("line", "int")) + child["columns"]