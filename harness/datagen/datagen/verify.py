"""Dataset verification.

Reads a generated dataset (JSONL or CSV; the manifest names the format) and
re-checks every invariant the generator promises, streaming: master data is
held in memory, stock events are replayed one date chunk at a time, and
return/payment cross-checks keep only the rows for referenced invoices.

Money and quantities are parsed back from their fixed-scale decimal
strings into integer minor units and milli units; floats never appear.
"""

import csv
import datetime as dt
import hashlib
import json
from pathlib import Path

from . import GENERATOR_NAME, tables
from .generate import (
    ADJUSTMENT_REASONS_IN,
    ADJUSTMENT_REASONS_OUT,
    _RETURN_DELAY_MAX_DAYS,
    BANK_ACCOUNT_ID,
    CASH_ACCOUNT_ID,
    OWNERS_ACCOUNT_ID,
)
from .money import round_half_even
from .profile import load_profile

STOCK_ORDER_INDEX = {name: rank for rank, name in enumerate(tables.STOCK_ORDER)}
RETURN_GRACE_DAYS = _RETURN_DELAY_MAX_DAYS


class VerifyError(Exception):
    """Raised once per dataset for structural problems that stop the run."""


def parse_money(text, where):
    """A fixed-scale 2dp string to paisa, rejecting scales and signs."""
    whole, dot, fraction = text.partition(".")
    if not dot or len(fraction) != 2 or not whole.isdigit() or not fraction.isdigit():
        raise VerifyError(f"{where}: not a 2dp money string: {text!r}")
    return int(whole) * 100 + int(fraction)


def parse_quantity(text, where):
    """A fixed-scale 3dp string to milli units, rejecting signs."""
    whole, dot, fraction = text.partition(".")
    if not dot or len(fraction) != 3 or not whole.isdigit() or not fraction.isdigit():
        raise VerifyError(f"{where}: not a 3dp quantity: {text!r}")
    return int(whole) * 1000 + int(fraction)


def parse_date(text, where):
    try:
        return dt.date.fromisoformat(text)
    except (ValueError, TypeError) as exc:
        raise VerifyError(f"{where}: not an ISO date: {text!r}") from exc


def _read_jsonl_rows(path):
    """Yield (line_number, row) for a JSONL file."""
    with open(path, encoding="utf-8") as handle:
        for line_number, text in enumerate(handle, 1):
            try:
                yield line_number, json.loads(text)
            except json.JSONDecodeError as exc:
                raise VerifyError(
                    f"{path.name} line {line_number}: invalid JSON: {exc}"
                ) from exc


def _read_csv_rows(path, table_name):
    """Yield (line_number, row) for a CSV parent file, scalars typed back."""
    names = [name for name, _kind in tables.TABLES[table_name]["columns"]]
    with open(path, encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle)
        try:
            header = next(reader)
        except StopIteration as exc:
            raise VerifyError(f"{path.name}: empty file") from exc
        if header != names:
            raise VerifyError(f"{path.name}: header {header!r} does not match the schema")
        for line_number, values in enumerate(reader, 2):
            if len(values) != len(names):
                raise VerifyError(
                    f"{path.name} line {line_number}: {len(values)} cells, expected {len(names)}"
                )
            yield line_number, dict(zip(names, values))


def _read_csv_children(path, table_name):
    """Yield (parent_id, line_number, row) from a CSV child file."""
    names = [name for name, _kind in tables.full_child_columns(table_name)]
    parent_key = tables.child_key(table_name)
    with open(path, encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle)
        try:
            header = next(reader)
        except StopIteration:
            return
        if header != names:
            raise VerifyError(f"{path.name}: header {header!r} does not match the schema")
        for line_number, values in enumerate(reader, 2):
            if len(values) != len(names):
                raise VerifyError(
                    f"{path.name} line {line_number}: {len(values)} cells, expected {len(names)}"
                )
            row = dict(zip(names, values))
            yield row.pop(parent_key), line_number, row


def read_dataset(out_dir):
    """Read manifest.json and return it after schema-level guards."""
    manifest_path = out_dir / "manifest.json"
    if not manifest_path.is_file():
        raise VerifyError(f"{out_dir}: no manifest.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for key in (
        "generator", "generator_version", "profile", "profile_sha256",
        "seed", "format", "files",
    ):
        if key not in manifest:
            raise VerifyError(f"manifest.json: missing {key!r}")
    if manifest["generator"] != GENERATOR_NAME:
        raise VerifyError(f"manifest.json: unknown generator {manifest['generator']!r}")
    if isinstance(manifest["seed"], bool) or not isinstance(manifest["seed"], int):
        raise VerifyError("manifest.json: 'seed' must be an integer")
    if manifest["format"] not in ("jsonl", "csv"):
        raise VerifyError(f"manifest.json: unknown format {manifest['format']!r}")
    return manifest


def _rows(out_dir, table_name, mode):
    """A parent table's rows in the shape both formats produce.

    JSONL keeps child lines inline; CSV joins the child file's consecutive
    rows back onto every parent.
    """
    suffix = ".jsonl" if mode == "jsonl" else ".csv"
    path = out_dir / f"{table_name}{suffix}"
    spec = tables.TABLES[table_name]
    lines_field = spec["lines_field"]
    if mode == "jsonl":
        for line_number, row in _read_jsonl_rows(path):
            if not isinstance(row, dict):
                raise VerifyError(f"{path.name} line {line_number}: not a JSON object")
            if lines_field is not None:
                if not isinstance(row.get(lines_field), list):
                    raise VerifyError(
                        f"{path.name} line {line_number}: {lines_field!r} must be a list"
                    )
            yield line_number, row
        return
    child = spec["child"]
    id_field = spec["columns"][0][0]
    pending = None
    children = None
    child_path = None
    if child is not None:
        child_path = out_dir / f"{child['name']}.csv"
        children = _read_csv_children(child_path, table_name)
        pending = next(children, None)
    for line_number, row in _read_csv_rows(path, table_name):
        lines = [] if lines_field is not None else None
        if pending is not None:
            while pending is not None and pending[0] == row[id_field]:
                lines.append(pending[2])
                pending = next(children, None)
            if pending is not None and pending[0] < row[id_field]:
                raise VerifyError(f"{child_path.name}: rows not grouped by parent")
        if lines_field is not None:
            row[lines_field] = lines
        yield line_number, row


def _as_int(value, where) -> int:
    """Accept a JSON int or a CSV string for a typed int column."""
    if isinstance(value, bool) or not isinstance(value, (int, str)):
        raise VerifyError(f"{where}: not an integer: {value!r}")
    if isinstance(value, str):
        if not value.isdigit():
            raise VerifyError(f"{where}: not an integer: {value!r}")
        return int(value)
    return value


def _to_boolean(value, where) -> bool:
    """Accept a JSON bool or the CSV strings true/false."""
    if isinstance(value, bool):
        return value
    if value in ("true", "false"):
        return value == "true"
    raise VerifyError(f"{where}: not a boolean: {value!r}")


def _load_master(out_dir, mode, profile, errors):
    """Load master data and check counts, uniqueness and references."""
    master = {
        "companies": {},  # id -> row
        "warehouses": {},  # id -> company_id
        "items": {},  # id -> standard rate in paisa
        "customers": {},  # id -> row
        "suppliers": {},  # id -> row
        "accounts": {},  # id -> row
    }
    expected = {
        "companies": profile.companies,
        "warehouses": profile.companies * profile.warehouses_per_company,
        "items": profile.items,
        "customers": profile.customers,
        "suppliers": profile.suppliers,
    }
    for table_name, table_spec in tables.MASTER.items():
        rows = 0
        for line_number, row in _rows(out_dir, table_name, mode):
            rows += 1
            where = f"{table_name} line {line_number}"
            for column, kind in table_spec["columns"]:
                if column not in row:
                    errors.append(f"{where}: missing column {column!r}")
                    row = None
                    break
                if kind == "bool":
                    _to_boolean(row[column], where)
            if row is None:
                continue
            row_id = row[table_spec["columns"][0][0]]
            if not str(row_id):
                errors.append(f"{where}: empty id")
                continue
            if row_id in master[table_name]:
                errors.append(f"{where}: duplicate id {row_id!r}")
                continue
            name = str(row.get("name") or "")
            if table_name != "accounts" and not name.strip():
                errors.append(f"{where}: empty name")
            if table_name == "warehouses":
                if row["company_id"] not in master["companies"]:
                    errors.append(f"{where}: warehouse before its company {row_id!r}")
                master["warehouses"][row_id] = row["company_id"]
            elif table_name == "items":
                rate = parse_money(row["standard_rate"], where)
                if rate < 100:
                    errors.append(f"{where}: standard_rate below PKR 1.00")
                master["items"][row_id] = rate
            else:
                master[table_name][row_id] = row
        if table_name in expected and rows != expected[table_name]:
            errors.append(
                f"{table_name}: {rows} rows, profile expects {expected[table_name]}"
            )
    return master


def _check_files(out_dir, manifest, errors):
    """The manifest's file list matches both the format's schema and bytes."""
    mode = manifest["format"]
    suffix = ".jsonl" if mode == "jsonl" else ".csv"
    expected_files = set()
    for name, spec in tables.TABLES.items():
        expected_files.add(f"{name}{suffix}")
        if mode == "csv" and spec["child"] is not None:
            expected_files.add(f"{spec['child']['name']}.csv")
    listed = manifest["files"]
    if not isinstance(listed, dict):
        errors.append("manifest.json: 'files' must be an object")
        return
    if set(listed) != expected_files:
        errors.append(
            f"manifest.json: files {sorted(listed)} do not match the {mode} schema files"
            f" {sorted(expected_files)}"
        )
    for name, entry in sorted(listed.items()):
        path = out_dir / name
        if not path.is_file():
            errors.append(f"manifest.json: {name} listed but missing on disk")
            continue
        if not isinstance(entry, dict) or "rows" not in entry or "sha256" not in entry:
            errors.append(f"manifest.json: {name} entry needs rows and sha256")
            continue
        digest = hashlib.sha256()
        if mode == "jsonl":
            rows = 0
            with open(path, "rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
                    rows += chunk.count(b"\n")
        else:
            with open(path, encoding="utf-8", newline="") as handle:
                reader = csv.reader(handle)
                next(reader, None)
                rows = sum(1 for _ in reader)
            with open(path, "rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
        if rows != entry["rows"]:
            errors.append(f"{name}: manifest says {entry['rows']} rows, found {rows}")
        actual = digest.hexdigest()
        if actual != entry["sha256"]:
            errors.append(f"{name}: digest {actual} does not match manifest")


def _check_opening(out_dir, mode, master, profile, errors):
    """Opening rows: date, flags, and the balance-sheet equation."""
    opening_date = profile.period_start
    sums = {}  # company_id -> {account_id: paisa, "receivable": paisa, "payable": paisa}
    for line_number, row in _rows(out_dir, "opening_stock", mode):
        where = f"opening_stock line {line_number}"
        if row["warehouse_id"] not in master["warehouses"] or row["item_id"] not in master["items"]:
            errors.append(f"{where}: unknown warehouse or item")
            continue
        if parse_date(row["date"], where) != opening_date:
            errors.append(f"{where}: opening row dated {row['date']}, expected {opening_date}")
        quantity = parse_quantity(row["quantity"], where)
        amount = parse_money(row["amount"], where)
        expected_amount = round_half_even(quantity * master["items"][row["item_id"]], 1000)
        if amount != expected_amount:
            errors.append(f"{where}: amount {row['amount']} != quantity * valuation_rate")
    for line_number, row in _rows(out_dir, "opening_invoice", mode):
        where = f"opening_invoice line {line_number}"
        flag = _to_boolean(row["is_opening"], where)
        if not flag:
            errors.append(f"{where}: is_opening must be true")
        if parse_date(row["date"], where) != opening_date:
            errors.append(f"{where}: opening row dated {row['date']}, expected {opening_date}")
        if row["direction"] not in ("receivable", "payable"):
            errors.append(f"{where}: direction {row['direction']!r}")
            continue
        expected_party_type = "customer" if row["direction"] == "receivable" else "supplier"
        if row["party_type"] != expected_party_type:
            errors.append(
                f"{where}: party_type {row['party_type']!r} for {row['direction']}"
            )
        parties = (
            master["customers"] if expected_party_type == "customer" else master["suppliers"]
        )
        if row["party_id"] not in parties:
            errors.append(f"{where}: unknown party {row['party_id']!r}")
        total = parse_money(row["total"], where)
        if total <= 0:
            errors.append(f"{where}: total must be positive")
        bucket = sums.setdefault(row["company_id"], {})
        key = "receivable" if row["direction"] == "receivable" else "payable"
        bucket[key] = bucket.get(key, 0) + total
    for line_number, row in _rows(out_dir, "opening_balance", mode):
        where = f"opening_balance line {line_number}"
        if parse_date(row["date"], where) != opening_date:
            errors.append(f"{where}: opening row dated {row['date']}, expected {opening_date}")
        if row["account_id"] not in master["accounts"]:
            errors.append(f"{where}: unknown account {row['account_id']!r}")
        bucket = sums.setdefault(row["company_id"], {})
        bucket[row["account_id"]] = parse_money(row["amount"], where)
    for company_id, bucket in sorted(sums.items()):
        liquid = bucket.get(CASH_ACCOUNT_ID, 0) + bucket.get(BANK_ACCOUNT_ID, 0)
        owners = bucket.get(OWNERS_ACCOUNT_ID)
        if owners is None:
            errors.append(f"{company_id}: no owners balance row")
            continue
        net = liquid + bucket.get("receivable", 0) - bucket.get("payable", 0)
        if owners != net:
            errors.append(
                f"{company_id}: owners {owners} != cash + bank + receivables - payables ({net})"
            )


_ROW_TABLES = (
    "opening_stock",
    "purchase_invoice",
    "stock_transfer",
    "stock_adjustment",
    "sales_invoice",
    "purchase_return",
    "sales_return",
)


def _replay_stock(out_dir, mode, master, profile, errors):
    """Replay per-(warehouse, item) stock in per-date stock order.

    Streams the stock-affecting tables one date chunk at a time; within a
    date they are applied in tables.STOCK_ORDER rank, exactly as the
    generator processes a day. Asserts stock never goes negative.
    """
    opening_date = profile.period_start
    streams = {}
    for table_name in _ROW_TABLES:
        iterator = iter(_rows(out_dir, table_name, mode))
        streams[table_name] = [next(iterator, None), iterator]
    ledger = {}
    seen = {name: set() for name in _ROW_TABLES}
    id_fields = {name: tables.TABLES[name]["columns"][0][0] for name in _ROW_TABLES}

    def head_date(cell, table_name):
        if cell[0] is None:
            return None
        return parse_date(cell[0][1]["date"], table_name)

    while True:
        current = None
        for table_name in _ROW_TABLES:
            date = head_date(streams[table_name], table_name)
            if date is not None and (current is None or date < current):
                current = date
        if current is None:
            break
        if current < opening_date:
            errors.append(f"stock events dated {current}, before the period start")
            return
        chunk = []
        for table_name in _ROW_TABLES:
            cell = streams[table_name]
            while cell[0] is not None:
                date = parse_date(cell[0][1]["date"], table_name)
                if date < current:
                    errors.append(f"{table_name} line {cell[0][0]}: rows out of date order")
                    return
                if date > current:
                    break
                chunk.append((STOCK_ORDER_INDEX[table_name], table_name, cell[0]))
                cell[0] = next(cell[1], None)
        chunk.sort(
            key=lambda entry: (entry[0], str(entry[2][1][id_fields[entry[1]]]))
        )
        for _, table_name, (line_number, row) in chunk:
            where = f"{table_name} line {line_number}"
            row_id = str(row[tables.TABLES[table_name]["columns"][0][0]])
            if row_id in seen[table_name]:
                errors.append(f"{where}: duplicate id {row_id!r}")
                continue
            seen[table_name].add(row_id)
            company_id = row["company_id"]
            if company_id not in master["companies"]:
                errors.append(f"{where}: unknown company {company_id!r}")
                continue
            for field in ("warehouse_id", "from_warehouse_id", "to_warehouse_id"):
                if field in row and row[field] not in master["warehouses"]:
                    errors.append(f"{where}: unknown warehouse {row[field]!r}")
            if table_name == "opening_stock":
                _apply_stock(ledger, errors, where, row)
            elif table_name == "purchase_invoice":
                _apply_invoice(ledger, errors, where, row, master, profile, +1)
            elif table_name == "sales_invoice":
                _apply_invoice(ledger, errors, where, row, master, profile, -1)
            elif table_name == "stock_transfer":
                _apply_transfer(ledger, errors, where, row, master, profile)
            elif table_name == "stock_adjustment":
                _apply_adjustment(ledger, errors, where, row, master, profile)
            elif table_name == "purchase_return":
                _apply_return(ledger, errors, where, row, master, profile, -1)
            elif table_name == "sales_return":
                _apply_return(ledger, errors, where, row, master, profile, +1)


def _adjust_stock(ledger, errors, where, warehouse_id, item_id, delta):
    """Apply a signed quantity change and fail on any negative stock."""
    key = (warehouse_id, item_id)
    have = ledger.get(key) or 0
    result = have + delta
    if result < 0:
        errors.append(
            f"{where}: stock of {item_id} at {warehouse_id} would go negative "
            f"({have} + {delta} milli)"
        )
        result = have  # keep replaying so one bad row does not cascade
    ledger[key] = result
    return result


def _apply_stock(ledger, errors, where, row):
    quantity = parse_quantity(row["quantity"], where)
    if quantity <= 0:
        errors.append(f"{where}: opening quantity must be positive")
    key = (row["warehouse_id"], row["item_id"])
    if key in ledger:
        errors.append(
            f"{where}: duplicate opening stock for {row['item_id']} at {row['warehouse_id']}"
        )
    ledger[key] = quantity


def _apply_invoice(ledger, errors, where, row, master, profile, sign):
    if "is_replenishment" in row:
        _to_boolean(row["is_replenishment"], where)
    party_field = "customer_id" if sign < 0 else "supplier_id"
    parties = master["customers"] if sign < 0 else master["suppliers"]
    if row[party_field] not in parties:
        errors.append(f"{where}: unknown {party_field} {row[party_field]!r}")
    date = parse_date(row["date"], where)
    if date < profile.period_start or date > profile.period_end:
        errors.append(f"{where}: invoice dated {date} outside the period")
    table = "sales_invoice" if sign < 0 else "purchase_invoice"
    lines = row[tables.TABLES[table]["lines_field"]]
    total = parse_money(row["total"], where)
    summed = 0
    for line in lines:
        quantity = parse_quantity(line["quantity"], where)
        rate = parse_money(line["rate"], where)
        amount = parse_money(line["amount"], where)
        summed += amount
        if line["item_id"] not in master["items"]:
            errors.append(f"{where}: unknown item {line['item_id']!r}")
            continue
        if quantity <= 0 or rate <= 0:
            errors.append(f"{where}: quantities and rates must be positive")
        if amount != round_half_even(quantity * rate, 1000):
            errors.append(f"{where}: line amount {line['amount']} != quantity * rate")
        _adjust_stock(ledger, errors, where, row["warehouse_id"], line["item_id"], sign * quantity)
    if summed != total:
        errors.append(f"{where}: total {row['total']} != lines sum")


def _apply_transfer(ledger, errors, where, row, master, profile):
    if row["from_warehouse_id"] == row["to_warehouse_id"]:
        errors.append(f"{where}: transfer between identical warehouses")
    if master["warehouses"].get(row["from_warehouse_id"]) != row["company_id"]:
        errors.append(f"{where}: from-warehouse belongs to another company")
    if master["warehouses"].get(row["to_warehouse_id"]) != row["company_id"]:
        errors.append(f"{where}: to-warehouse belongs to another company")
    date = parse_date(row["date"], where)
    if date < profile.period_start or date > profile.period_end:
        errors.append(f"{where}: transfer dated {date} outside the period")
    quantity = parse_quantity(row["quantity"], where)
    rate = parse_money(row["rate"], where)
    amount = parse_money(row["amount"], where)
    if row["item_id"] not in master["items"]:
        errors.append(f"{where}: unknown item {row['item_id']!r}")
        return
    if quantity <= 0:
        errors.append(f"{where}: transfer quantity must be positive")
    if rate != master["items"][row["item_id"]]:
        errors.append(f"{where}: transfer rate must be the item's standard rate")
    if amount != round_half_even(quantity * rate, 1000):
        errors.append(f"{where}: amount {row['amount']} != quantity * rate")
    _adjust_stock(ledger, errors, where, row["from_warehouse_id"], row["item_id"], -quantity)
    _adjust_stock(ledger, errors, where, row["to_warehouse_id"], row["item_id"], +quantity)


def _apply_adjustment(ledger, errors, where, row, master, profile):
    direction = row["direction"]
    reasons = ADJUSTMENT_REASONS_IN if direction == "in" else ADJUSTMENT_REASONS_OUT
    if direction not in ("in", "out"):
        errors.append(f"{where}: direction {direction!r} must be in or out")
    if row["reason_code"] not in reasons:
        errors.append(f"{where}: reason {row['reason_code']!r} not allowed for {direction}")
    if master["warehouses"].get(row["warehouse_id"]) != row["company_id"]:
        errors.append(f"{where}: warehouse belongs to another company")
    date = parse_date(row["date"], where)
    if date < profile.period_start or date > profile.period_end:
        errors.append(f"{where}: adjustment dated {date} outside the period")
    quantity = parse_quantity(row["quantity"], where)
    rate = parse_money(row["rate"], where)
    amount = parse_money(row["amount"], where)
    if rate <= 0 or quantity <= 0:
        errors.append(f"{where}: quantities and rates must be positive")
    if amount != round_half_even(quantity * rate, 1000):
        errors.append(f"{where}: amount {row['amount']} != quantity * rate")
    if row["item_id"] not in master["items"]:
        errors.append(f"{where}: unknown item {row['item_id']!r}")
        return
    _adjust_stock(
        ledger, errors, where, row["warehouse_id"], row["item_id"],
        quantity if direction == "in" else -quantity,
    )


def _apply_return(ledger, errors, where, row, master, profile, sign):
    date = parse_date(row["date"], where)
    if date > profile.period_end + dt.timedelta(days=RETURN_GRACE_DAYS):
        errors.append(f"{where}: return dated {date}, too far past the period end")
    if master["warehouses"].get(row["warehouse_id"]) != row["company_id"]:
        errors.append(f"{where}: warehouse belongs to another company")
    table = "sales_return" if sign > 0 else "purchase_return"
    lines = row[tables.TABLES[table]["lines_field"]]
    total = parse_money(row["total"], where)
    summed = 0
    for line in lines:
        quantity = parse_quantity(line["quantity"], where)
        rate = parse_money(line["rate"], where)
        amount = parse_money(line["amount"], where)
        summed += amount
        if _as_int(line["original_line"], where) < 1:
            errors.append(f"{where}: original_line must be 1-based")
        if quantity <= 0:
            errors.append(f"{where}: returned quantity must be positive")
        if amount != round_half_even(quantity * rate, 1000):
            errors.append(f"{where}: line amount {line['amount']} != quantity * rate")
        if line["item_id"] not in master["items"]:
            errors.append(f"{where}: unknown item {line['item_id']!r}")
            continue
        _adjust_stock(ledger, errors, where, row["warehouse_id"], line["item_id"], sign * quantity)
    if summed != total:
        errors.append(f"{where}: total {row['total']} != lines sum")


def _check_returns(out_dir, mode, errors):
    """Returns must reference real original lines and never over-return.

    Holds only the returns and the original invoices that are returned
    against — never whole transaction tables.
    """
    for return_table, invoice_table, party_field in (
        ("purchase_return", "purchase_invoice", "supplier_id"),
        ("sales_return", "sales_invoice", "customer_id"),
    ):
        wanted = {}  # original invoice id -> {line number: total returned milli}
        jobs = []  # (invoice id, line no, milli, item, rate text, return date, where)
        for line_number, row in _rows(out_dir, return_table, mode):
            where = f"{return_table} line {line_number}"
            original = row.get("original_invoice_id")
            if not original:
                errors.append(f"{where}: missing original_invoice_id")
                continue
            bucket = wanted.setdefault(original, {})
            for line in row[tables.TABLES[return_table]["lines_field"]]:
                line_no = _as_int(line["original_line"], where)
                quantity = parse_quantity(line["quantity"], where)
                bucket[line_no] = bucket.get(line_no, 0) + quantity
                jobs.append(
                    (original, line_no, quantity, line["item_id"], line["rate"],
                     row["date"], row[party_field], row["warehouse_id"], where)
                )
        if not wanted:
            continue
        originals = {}
        id_field = tables.TABLES[invoice_table]["columns"][0][0]
        for line_number, invoice in _rows(out_dir, invoice_table, mode):
            invoice_id = invoice[id_field]
            if invoice_id in wanted:
                originals[invoice_id] = (
                    invoice[party_field], invoice["warehouse_id"], invoice["date"],
                    [
                        (line["item_id"], parse_quantity(line["quantity"], f"{invoice_table} {invoice_id}"),
                         line["rate"])
                        for line in invoice["lines"]
                    ],
                )
        for invoice_id, line_no, _quantity, item_id, rate_text, date, party, warehouse, where in jobs:
            original = originals.get(invoice_id)
            if original is None:
                errors.append(f"{where}: original invoice {invoice_id} not found")
                continue
            original_party, original_warehouse, invoice_date, lines = original
            if date <= invoice_date:
                errors.append(f"{where}: return dated {date} not after its invoice {invoice_date}")
            if party != original_party:
                errors.append(
                    f"{where}: party {party!r} differs from the invoice party {original_party!r}"
                )
            if warehouse != original_warehouse:
                errors.append(
                    f"{where}: warehouse {warehouse!r} differs from the invoice warehouse"
                )
            if line_no < 1 or line_no > len(lines):
                errors.append(f"{where}: original_line {line_no} of {invoice_id} does not exist")
                continue
            original_item, _original_quantity, original_rate = lines[line_no - 1]
            if item_id != original_item:
                errors.append(
                    f"{where}: returned {item_id} but line {line_no} of {invoice_id} is {original_item}"
                )
            if rate_text != original_rate:
                errors.append(
                    f"{where}: rate {rate_text} differs from the invoice line rate {original_rate}"
                )
        for invoice_id, per_line in wanted.items():
            if invoice_id not in originals:
                continue
            lines = originals[invoice_id][3]
            for line_no, total_returned in sorted(per_line.items()):
                if not 1 <= line_no <= len(lines):
                    continue
                original_quantity = lines[line_no - 1][1]
                if total_returned > original_quantity:
                    errors.append(
                        f"{return_table}: {invoice_id} line {line_no}: returned "
                        f"{total_returned} milli exceeds original {original_quantity} milli"
                    )


def _check_journals(out_dir, mode, master, profile, errors):
    """Journal entries balance exactly and only use master accounts."""
    table = tables.TABLES["journal_entry"]
    for line_number, row in _rows(out_dir, "journal_entry", mode):
        where = f"journal_entry line {line_number}"
        date = parse_date(row["date"], where)
        if date < profile.period_start or date > profile.period_end:
            errors.append(f"{where}: entry dated {date} outside the period")
        if row["company_id"] not in master["companies"]:
            errors.append(f"{where}: unknown company {row['company_id']!r}")
        lines = row[table["lines_field"]]
        if len(lines) < 2:
            errors.append(f"{where}: needs at least two lines")
        debits = credits = 0
        for line in lines:
            debit = parse_money(line["debit"], where)
            credit = parse_money(line["credit"], where)
            if debit > 0 and credit > 0:
                errors.append(f"{where}: line has both debit and credit")
            debits += debit
            credits += credit
            if line["account_id"] not in master["accounts"]:
                errors.append(f"{where}: unknown account {line['account_id']!r}")
        if debits != credits:
            errors.append(f"{where}: unbalanced ({debits} debit vs {credits} credit)")
        if debits + credits == 0:
            errors.append(f"{where}: zero-value entry")


def _check_payments(out_dir, mode, master, profile, errors):
    """Payments: one invoice each, fully paid, never double-paid."""
    referenced = set()
    payments = []  # (invoice id, amount paisa, payment date, direction, party fields, where)
    table = tables.TABLES["payment"]
    for line_number, row in _rows(out_dir, "payment", mode):
        where = f"payment line {line_number}"
        if row["direction"] not in ("receivable", "payable"):
            errors.append(f"{where}: direction {row['direction']!r}")
            continue
        if row["party_type"] not in ("customer", "supplier"):
            errors.append(f"{where}: party_type {row['party_type']!r}")
            continue
        parties = master["customers"] if row["party_type"] == "customer" else master["suppliers"]
        if row["party_id"] not in parties:
            errors.append(f"{where}: unknown party {row['party_id']!r}")
            continue
        date = parse_date(row["date"], where)
        if date > profile.period_end + dt.timedelta(days=profile.delay_max_days):
            errors.append(
                f"{where}: payment dated {date}, past the period plus the payment window"
            )
        amount = parse_money(row["amount"], where)
        allocations = row[table["lines_field"]]
        if len(allocations) != 1:
            errors.append(f"{where}: exactly one allocation per payment")
            continue
        allocation = allocations[0]
        invoice_id = allocation["invoice_id"]
        if not invoice_id:
            errors.append(f"{where}: missing allocation invoice")
            continue
        if parse_money(allocation["amount"], where) != amount:
            errors.append(f"{where}: allocation {allocation['amount']} != payment amount")
        referenced.add((invoice_id, row["direction"]))
        payments.append((invoice_id, amount, date, row["direction"],
                         row["party_type"], row["party_id"], where))
    invoices = {}
    pay_ids = {invoice_id for invoice_id, direction in referenced if direction == "receivable"}
    bill_ids = {invoice_id for invoice_id, direction in referenced if direction == "payable"}
    for table_name, direction in (("sales_invoice", "receivable"),
                                  ("purchase_invoice", "payable")):
        id_field = tables.TABLES[table_name]["columns"][0][0]
        wrong_ids = pay_ids if direction == "payable" else bill_ids
        wanted_ids = pay_ids if direction == "receivable" else bill_ids
        for line_number, invoice in _rows(out_dir, table_name, mode):
            invoice_id = invoice[id_field]
            if invoice_id in wrong_ids:
                errors.append(
                    f"{table_name} {invoice_id}: paid as "
                    f"{'payable' if direction == 'receivable' else 'receivable'}, "
                    f"but it is a {'sales' if direction == 'receivable' else 'purchase'} invoice"
                )
            if invoice_id in wanted_ids:
                invoices[(invoice_id, direction)] = (
                    invoice.get("customer_id") or invoice.get("supplier_id"),
                    parse_money(invoice["total"], f"{table_name} {invoice_id}"),
                    invoice["date"],
                )
    seen = set()
    for invoice_id, amount, date, direction, party_type, party_id, where in payments:
        invoice = invoices.get((invoice_id, direction))
        if invoice is None:
            errors.append(f"{where}: invoice {invoice_id} not found")
            continue
        party, total, invoice_date = invoice
        if amount != total:
            errors.append(f"{where}: payment {amount} != invoice total {total}")
        if invoice_id in seen:
            errors.append(f"{where}: invoice {invoice_id} settled twice")
        seen.add(invoice_id)
        paid_on = parse_date(invoice_date, where)
        if date < paid_on:
            errors.append(f"{where}: payment predates the invoice")
        delay = (date - paid_on).days
        if not profile.delay_min_days <= delay <= profile.delay_max_days:
            errors.append(
                f"{where}: payment {delay} days after the invoice, "
                f"outside the profile's payment delay window"
            )
        if party_type != ("customer" if direction == "receivable" else "supplier"):
            errors.append(f"{where}: party_type {party_type} does not match {direction}")
        if party != party_id:
            errors.append(f"{where}: party {party_id!r} != invoice party {party!r}")


def verify_dataset(out_dir):
    """Verify one generated dataset; return the list of violations found.

    Structural problems that stop every later check raise VerifyError;
    content violations are accumulated and returned so the whole report
    is printed in one run.
    """
    out_dir = Path(out_dir)
    if not out_dir.is_dir():
        raise VerifyError(f"{out_dir}: not a directory")
    manifest = read_dataset(out_dir)
    errors = []
    _check_files(out_dir, manifest, errors)
    mode = manifest["format"]
    try:
        profile = load_profile(manifest["profile"])
    except (OSError, ValueError) as exc:
        raise VerifyError(
            f"manifest.json: profile {manifest['profile']!r} could not be loaded: {exc}"
        ) from exc
    if manifest["profile_sha256"] != profile.sha256:
        errors.append("manifest.json: profile_sha256 does not match the loaded profile")
    master = _load_master(out_dir, mode, profile, errors)
    _check_opening(out_dir, mode, master, profile, errors)
    _replay_stock(out_dir, mode, master, profile, errors)
    _check_journals(out_dir, mode, master, profile, errors)
    _check_returns(out_dir, mode, errors)
    _check_payments(out_dir, mode, master, profile, errors)
    return errors