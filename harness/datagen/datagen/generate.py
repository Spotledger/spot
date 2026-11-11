"""Dataset generation.

The generator builds the master data once from per-entity RNG streams and
then streams one output row per table entry in a single chronological
pass: for every day of the period the generator writes that day's journal
entries, planned purchase invoices, stock transfers, stock adjustments,
sales invoices, scheduled purchase and sales returns, and the payments
falling due on that day — in that order — then releases the day. Only
master data, the small per-day plans, the payment/return delay buckets and
the in-memory stock ledger are kept in memory, so output streams at any
profile size. The per-(warehouse, item) ledger keeps milli-unit stock; a
sale, transfer-out, adjustment-out or purchase return that would push a
warehouse negative first emits automatic replenishment purchase invoices
(the ``is_replenishment`` flag, one per short item, processed in stock
order), so stock can never go below zero anywhere in the dataset.

Determinism and stream isolation: every table has its own RNG stream named
after it, so v2 additions never moved T1 tables' bytes, and the whole
output is byte-identical for the same profile bytes, seed, and
GENERATOR_VERSION.
"""

import datetime as dt
import time
from pathlib import Path

from . import GENERATOR_NAME, GENERATOR_VERSION, names, tables
from .money import format_money, format_quantity, round_half_even
from .profile import Profile
from .sampling import (
    stream_rng,
    triangular_cum_weights,
    weighted_index,
    zipf_cum_weights,
)
from .writer import RowSink, write_manifest

COMPANY_CURRENCY = "PKR"
ITEM_UNITS = ("unit", "kg", "litre", "metre", "box")
_RATE_MIN_PAISA = 100  # PKR 1.00
_RATE_MAX_PAISA = 500_000  # PKR 5,000.00
_QTY_MIN_MILLI = 1
_QTY_MAX_MILLI = 20_000  # 20.000 units
_TRANSFER_QTY_MAX_MILLI = 10_000  # 10.000 units
_ADJUST_QTY_MAX_MILLI = 5_000  # 5.000 units
_OPENING_INVOICE_MIN_PAISA = 10_000  # PKR 100.00
_OPENING_INVOICE_MAX_PAISA = 25_000_000  # PKR 250,000.00
_CASH_ACCOUNT_MIN_PAISA = 100_000  # PKR 1,000.00
_CASH_ACCOUNT_MAX_PAISA = 50_000_000  # PKR 500,000.00
_JOURNAL_AMOUNT_MIN_PAISA = 500_000  # PKR 5,000.00
_JOURNAL_AMOUNT_MAX_PAISA = 50_000_000  # PKR 500,000.00
_RETURN_DELAY_MIN_DAYS = 20
_RETURN_DELAY_MAX_DAYS = 90

# The provisional chart of accounts: fixed rows, the same for every
# company, so journal entries and opening balances reference stable ids.
CHART_OF_ACCOUNTS = (
    ("AC-0001", "1000", "Cash on Hand", "asset"),
    ("AC-0002", "1010", "Bank Account", "asset"),
    ("AC-0003", "1100", "Receivables", "asset"),
    ("AC-0004", "1200", "Stock on Hand", "asset"),
    ("AC-0005", "2000", "Payables", "liability"),
    ("AC-0006", "3000", "Owner Capital", "equity"),
    ("AC-0007", "4000", "Sales Revenue", "income"),
    ("AC-0008", "5000", "Goods Purchases", "expense"),
    ("AC-0009", "6000", "Salaries Expense", "expense"),
    ("AC-0010", "6010", "Rent Expense", "expense"),
    ("AC-0011", "6020", "Utilities Expense", "expense"),
    ("AC-0012", "6090", "Other Operating Expense", "expense"),
)
EXPENSE_ACCOUNTS = ("AC-0008", "AC-0009", "AC-0010", "AC-0011", "AC-0012")
CASH_ACCOUNT_ID = "AC-0001"
BANK_ACCOUNT_ID = "AC-0002"
OWNERS_ACCOUNT_ID = "AC-0006"  # Owner Capital: the net-effect opening row
MEMO_PHRASES = {
    "AC-0008": ("Goods bought on account", "Restock supplies on credit"),
    "AC-0009": ("Staff salaries", "Monthly payroll"),
    "AC-0010": ("Office rent", "Warehouse rent"),
    "AC-0011": ("Electricity bill", "Water and internet"),
    "AC-0012": ("Repairs and upkeep", "Local travel"),
}
ADJUSTMENT_REASONS_IN = ("count-correction",)
ADJUSTMENT_REASONS_OUT = ("damaged", "expired", "lost")


class GeneratorError(Exception):
    """Raised when the requested output location cannot be used."""


def _plan_days(start: dt.date, end: dt.date, total: int, seasonality) -> list:
    """Spread `total` transactions over the period's days proportionally to
    the monthly seasonality weights, by the largest-remainder method: the
    split is exact, integer-only and deterministic, so the day's volume is
    known before any row of that day is generated."""
    days = []
    day = start
    while day <= end:
        days.append(day)
        day += dt.timedelta(days=1)
    weights = [seasonality[day.month - 1] for day in days]
    weight_sum = sum(weights)
    base = [total * weight // weight_sum for weight in weights]
    remainder = total - sum(base)
    order = sorted(
        range(len(days)),
        key=lambda i: (-(total * weights[i] - base[i] * weight_sum), i),
    )
    for i in order[:remainder]:
        base[i] += 1
    return [(days[i], base[i]) for i in range(len(days))]


def _designated(index: int, count: int, target: int) -> bool:
    """Systematic (Bresenham) spread: exactly `target` of `count` positions,
    evenly spaced by index, are selected. Pure arithmetic, so scheduling
    never depends on RNG order and every count is exact."""
    if target <= 0 or count <= 0:
        return False
    return (index + 1) * target // count > index * target // count


def _build_master(profile: Profile, seed: int) -> dict:
    """Every master table, generated once from its own RNG stream, in the
    fixed stream order company, warehouse, item, customer, supplier; the
    provisional chart of accounts is fixed data, not generated."""
    rng = stream_rng(seed, "company")
    used = set()
    companies = []
    for index in range(1, profile.companies + 1):
        companies.append({
            "company_id": f"CO-{index:04d}",
            "name": names.unique_name(rng, used, names.company_name),
            "currency": COMPANY_CURRENCY,
        })

    rng = stream_rng(seed, "warehouse")
    used = set()
    warehouses = []
    warehouse_ids_by_company = {}
    sequence = 0
    for company in companies:
        owned = []
        for _ in range(profile.warehouses_per_company):
            sequence += 1
            warehouse_id = f"WH-{sequence:05d}"
            owned.append(warehouse_id)
            warehouses.append({
                "warehouse_id": warehouse_id,
                "company_id": company["company_id"],
                "name": names.unique_name(
                    rng, used, lambda rng_, seq=sequence: names.warehouse_name(rng_, seq)
                ),
            })
        warehouse_ids_by_company[company["company_id"]] = owned

    rng = stream_rng(seed, "item")
    used = set()
    items = []
    item_rates = []
    for index in range(1, profile.items + 1):
        rate = rng.randrange(_RATE_MIN_PAISA, _RATE_MAX_PAISA + 1)
        items.append({
            "item_id": f"IT-{index:06d}",
            "code": f"ITM-{index:06d}",
            "name": names.unique_name(rng, used, names.item_name),
            "unit": rng.choice(ITEM_UNITS),
            "standard_rate": format_money(rate),
        })
        item_rates.append(rate)

    rng = stream_rng(seed, "customer")
    used = set()
    customers = []
    for index in range(1, profile.customers + 1):
        customers.append({
            "customer_id": f"CU-{index:06d}",
            "name": names.unique_name(rng, used, names.party_name),
        })

    rng = stream_rng(seed, "supplier")
    used = set()
    suppliers = []
    for index in range(1, profile.suppliers + 1):
        suppliers.append({
            "supplier_id": f"SU-{index:05d}",
            "name": names.unique_name(rng, used, names.party_name),
        })

    accounts = [
        {"account_id": account_id, "code": code, "name": name, "kind": kind}
        for account_id, code, name, kind in CHART_OF_ACCOUNTS
    ]
    return {
        "companies": companies,
        "warehouses": warehouses,
        "items": items,
        "customers": customers,
        "suppliers": suppliers,
        "accounts": accounts,
        "warehouse_ids_by_company": warehouse_ids_by_company,
        "item_rates": item_rates,
    }


def _make_entry(rng, company_ids, day, entry_seq) -> dict:
    """One balanced journal entry: 2-4 expense debit lines that sum to the
    drawn total, and one credit line to cash or bank."""
    company_id = company_ids[rng.randrange(len(company_ids))]
    account_id = EXPENSE_ACCOUNTS[rng.randrange(len(EXPENSE_ACCOUNTS))]
    phrase, alternative = MEMO_PHRASES[account_id]
    memo = phrase if rng.randrange(2) == 0 else alternative
    total = rng.randrange(_JOURNAL_AMOUNT_MIN_PAISA, _JOURNAL_AMOUNT_MAX_PAISA + 1)
    debit_count = rng.randrange(2, 5)
    debits = []
    left = total
    for index in range(debit_count - 1):
        share = rng.randrange(1, left // (debit_count - index) + 1)
        debits.append(share)
        left -= share
    debits.append(left)
    lines = [
        {"account_id": account_id, "debit": format_money(share), "credit": "0.00"}
        for share in debits
    ]
    credit_account = (CASH_ACCOUNT_ID, BANK_ACCOUNT_ID)[rng.randrange(2)]
    lines.append({"account_id": credit_account, "debit": "0.00", "credit": format_money(total)})
    return {
        "entry_id": f"JE-{entry_seq:07d}",
        "company_id": company_id,
        "date": day.isoformat(),
        "memo": memo,
        "lines": lines,
    }


def _make_invoice(
    rng, invoice_id, party_field, party_id, company_id, warehouse_id, day,
    item_cum, item_ids, item_rates, line_cum, lines_min,
) -> tuple:
    """One invoice row plus its raw lines: the raw lines carry the item
    index, the milli quantity and the milli rate that stock keeping needs,
    so returns can point at the exact original line. Draw order per line:
    item, rate spread, quantity."""
    raw = []
    line_count = lines_min + weighted_index(rng, line_cum)
    lines = []
    total = 0
    for _ in range(line_count):
        item_index = weighted_index(rng, item_cum)
        base = item_rates[item_index]
        spread = base // 10
        rate = base + rng.randrange(-spread, spread + 1)
        if rate < 1:
            rate = 1
        quantity = rng.randrange(_QTY_MIN_MILLI, _QTY_MAX_MILLI + 1)
        amount = round_half_even(quantity * rate, 1000)
        lines.append({
            "item_id": item_ids[item_index],
            "quantity": format_quantity(quantity),
            "rate": format_money(rate),
            "amount": format_money(amount),
        })
        total += amount
        raw.append((item_index, quantity, rate))
    row = {
        "invoice_id": invoice_id,
        "company_id": company_id,
        "warehouse_id": warehouse_id,
        party_field: party_id,
        "date": day.isoformat(),
        "total": format_money(total),
        "lines": lines,
    }
    return row, raw


def _write_opening(sink, profile, master, seed) -> tuple:
    """Write the three opening tables (all dated on the first day of the
    period) and return (stock_ledger, openers, balances). stock_ledger maps
    (warehouse_id, item_id) to milli quantity; openers are the opening
    receivable/payable invoice rows; balances are the cash, bank and
    net-effect equity rows. Draw order: one stock quantity per covered
    pair (opening_stock stream, pairs in row order), amounts for the
    receivable and then payable opening invoices (opening_invoice
    stream), cash and bank amounts per company (opening balance stream).

    Every covered pair keeps an opening stock row and every party share
    gets one opening invoice, so the verifier can check the designations
    arithmetically exactly like the rest of the designations."""
    opening_date = profile.period_start
    company_ids = [company["company_id"] for company in master["companies"]]
    warehouses_by_company = master["warehouse_ids_by_company"]
    item_ids = [item["item_id"] for item in master["items"]]
    item_rates = master["item_rates"]
    customer_ids = [customer["customer_id"] for customer in master["customers"]]
    supplier_ids = [supplier["supplier_id"] for supplier in master["suppliers"]]

    total_pairs = len(company_ids) * profile.warehouses_per_company * profile.items
    target_pairs = total_pairs * profile.opening_stock_coverage_per_mille // 1000
    rng = stream_rng(seed, "opening_stock")
    ledger = {}
    stock_seq = 0
    index = 0
    for company_id in company_ids:
        for warehouse_id in warehouses_by_company[company_id]:
            for item_index, item_id in enumerate(item_ids):
                quantity = profile.opening_stock_min_milli + rng.randrange(
                    profile.opening_stock_max_milli - profile.opening_stock_min_milli + 1
                )
                if _designated(index, total_pairs, target_pairs):
                    key = (warehouse_id, item_id)
                    ledger[key] = quantity
                    stock_seq += 1
                    rate = item_rates[item_index]
                    sink.write("opening_stock", {
                        "opening_stock_id": f"OS-{stock_seq:06d}",
                        "company_id": company_id,
                        "warehouse_id": warehouse_id,
                        "item_id": item_id,
                        "date": opening_date.isoformat(),
                        "quantity": format_quantity(quantity),
                        "valuation_rate": format_money(rate),
                        "amount": format_money(round_half_even(quantity * rate, 1000)),
                    })
                index += 1

    invoice_rng = stream_rng(seed, "opening_invoice")
    openers = []
    receivable_milli = {}
    payable_milli = {}
    for direction, party_field, party_ids, share in (
        ("receivable", "customer", customer_ids, profile.opening_receivable_per_mille),
        ("payable", "supplier", supplier_ids, profile.opening_payable_per_mille),
    ):
        sums = payable_milli if direction == "payable" else receivable_milli
        pairs = [
            (company_id, party_id)
            for company_id in company_ids
            for party_id in party_ids
        ]
        target = len(pairs) * share // 1000
        for index, (company_id, party_id) in enumerate(pairs):
            if not _designated(index, len(pairs), target):
                continue
            amount = invoice_rng.randrange(
                _OPENING_INVOICE_MIN_PAISA, _OPENING_INVOICE_MAX_PAISA + 1
            )
            sums[company_id] = sums.get(company_id, 0) + amount
            openers.append({
                "company_id": company_id,
                "direction": direction,
                "party_type": party_field,
                "party_id": party_id,
                "date": opening_date.isoformat(),
                "total": format_money(amount),
            })

    balance_rng = stream_rng(seed, "opening_balance")
    cash_milli = {}
    bank_milli = {}
    for company_id in company_ids:
        for account_id, sums in ((CASH_ACCOUNT_ID, cash_milli), (BANK_ACCOUNT_ID, bank_milli)):
            amount = balance_rng.randrange(_CASH_ACCOUNT_MIN_PAISA, _CASH_ACCOUNT_MAX_PAISA + 1)
            sums[company_id] = sums.get(company_id, 0) + amount
    balances = []
    for company_id in company_ids:
        for account_id, sums in ((CASH_ACCOUNT_ID, cash_milli), (BANK_ACCOUNT_ID, bank_milli)):
            balances.append({
                "company_id": company_id,
                "account_id": account_id,
                "date": opening_date.isoformat(),
                "amount": format_money(sums[company_id]),
            })
    # One net-effect row per company pegs the rest of the opening balance
    # sheet: what the owners put in, equal to assets minus the liabilities
    # the opening rows themselves carry.
    for company_id in company_ids:
        owed = receivable_milli.get(company_id, 0) - payable_milli.get(company_id, 0)
        net = cash_milli[company_id] + bank_milli[company_id] + owed
        balances.append({
            "company_id": company_id,
            "account_id": OWNERS_ACCOUNT_ID,
            "date": opening_date.isoformat(),
            "amount": format_money(net),
        })
    return ledger, openers, balances


def generate_dataset(profile: Profile, seed: int, out_dir, mode: str = "jsonl") -> dict:
    """Generate the whole dataset into an absent or empty out_dir and return
    the output summary. Deterministic: the same profile bytes, seed and
    version write the same bytes.

    Master data and the opening rows are written first, each table from its
    own named stream. The transaction days then stream through in one
    chronological pass; every day processes, in this order and each with its
    own stream: (1) journal entries; (2) planned purchase invoices, whose
    stock lands in the ledger and whose designated shares schedule their
    payments and returns; (3) stock transfers, replenishing the source
    warehouse first when it would go short; (4) stock adjustments; (5) sales
    invoices, their lines replenishing as they deduct; (6) purchase returns
    due that day; (7) sales returns due that day; (8) the payments falling
    due that day, each fully paying one invoice. Whatever falls due after
    the last day is flushed at the end, dated after the period. The
    no-negative-stock rule: a deduction is always preceded, in per-date
    stock order, by replenishment purchase invoices to the same warehouse."""
    started = time.monotonic()
    out_dir = Path(out_dir)
    if out_dir.exists():
        if not out_dir.is_dir():
            raise GeneratorError(f"--out {out_dir} exists and is not a directory")
        if any(out_dir.iterdir()):
            raise GeneratorError(
                f"--out {out_dir} is not empty; pick a fresh directory"
            )
    else:
        out_dir.mkdir(parents=True)

    master = _build_master(profile, seed)
    company_ids = [company["company_id"] for company in master["companies"]]
    item_ids = [item["item_id"] for item in master["items"]]
    customer_ids = [customer["customer_id"] for customer in master["customers"]]
    supplier_ids = [supplier["supplier_id"] for supplier in master["suppliers"]]
    warehouses_by_company = master["warehouse_ids_by_company"]
    item_rates = master["item_rates"]

    with RowSink(out_dir, mode) as sink:
        for name in tables.MASTER_NAMES:
            for row in master[name]:
                sink.write(name, row)
        ledger, openers, balances = _write_opening(sink, profile, master, seed)
        for index, opener in enumerate(openers, start=1):
            row = dict(opener)
            row["invoice_id"] = f"OI-{index:06d}"
            row["is_opening"] = True
            sink.write("opening_invoice", row)
        for index, balance in enumerate(balances, start=1):
            row = dict(balance)
            row["opening_balance_id"] = f"OB-{index:04d}"
            sink.write("opening_balance", row)
        sales_by_day = dict(_plan_days(
            profile.period_start, profile.period_end, profile.sales_invoices,
            profile.monthly_seasonality,
        ))
        purchases_by_day = dict(_plan_days(
            profile.period_start, profile.period_end, profile.purchase_invoices,
            profile.monthly_seasonality,
        ))
        journals_by_day = dict(_plan_days(
            profile.period_start, profile.period_end, profile.journal_entries,
            profile.monthly_seasonality,
        ))
        transfers_by_day = dict(_plan_days(
            profile.period_start, profile.period_end, profile.stock_transfers,
            profile.monthly_seasonality,
        ))
        adjustments_by_day = dict(_plan_days(
            profile.period_start, profile.period_end, profile.stock_adjustments,
            profile.monthly_seasonality,
        ))

        item_cum = zipf_cum_weights(profile.items, profile.item_popularity_skew)
        customer_cum = zipf_cum_weights(
            profile.customers, profile.customer_activity_skew
        )
        line_cum = triangular_cum_weights(
            profile.lines_min, profile.lines_peak, profile.lines_max
        )
        journal_rng = stream_rng(seed, "journal_entry")
        purchase_rng = stream_rng(seed, "purchase_invoice")
        transfer_rng = stream_rng(seed, "stock_transfer")
        adjust_rng = stream_rng(seed, "stock_adjustment")
        sales_rng = stream_rng(seed, "sales_invoice")
        payment_rng = stream_rng(seed, "payment")
        purchase_return_rng = stream_rng(seed, "purchase_return")
        sales_return_rng = stream_rng(seed, "sales_return")
        replenishment_rng = stream_rng(seed, "replenishment")

        target_paid_sales = (
            profile.sales_invoices * profile.paid_share_per_mille // 1000
        )
        target_paid_purchases = (
            profile.purchase_invoices * profile.paid_share_per_mille // 1000
        )
        target_pr = (
            profile.purchase_invoices * profile.purchase_return_per_mille // 1000
        )
        target_sr = (
            profile.sales_invoices * profile.sales_return_per_mille // 1000
        )

        journal_seq = purchase_seq = transfer_seq = adjustment_seq = 0
        sales_seq = payment_seq = pr_seq = sr_seq = 0
        purchase_plan_index = 0
        sales_plan_index = 0
        due_buckets = {}          # payment due date -> payment jobs
        purchase_return_jobs = {}  # return date -> purchase return jobs
        sales_return_jobs = {}     # return date -> sales return jobs

        def _replenish(company_id, warehouse_id, item_index, shortfall, day):
            """One replenishment purchase invoice covering a shortfall plus
            a top-up inside the opening-stock window; the ledger rises
            before the row is written. Draws: top-up quantity, rate
            spread, supplier picking."""
            nonlocal purchase_seq
            key = (warehouse_id, item_ids[item_index])
            short = shortfall - ledger.get(key, 0)
            if short <= 0:
                return
            # Trailing returns flush after the period, but a purchase invoice
            # must be dated inside the period (verify pins invoices to it), so
            # the replenishment order is dated on the period's last day and the
            # stock still rises before the trailing consumption is replayed.
            invoice_date = min(day, profile.period_end)
            topup = profile.opening_stock_min_milli + replenishment_rng.randrange(
                profile.opening_stock_max_milli - profile.opening_stock_min_milli + 1
            )
            quantity = short + topup
            base = item_rates[item_index]
            spread = base // 10
            rate = base + replenishment_rng.randrange(-spread, spread + 1)
            if rate < 1:
                rate = 1
            supplier_id = supplier_ids[
                replenishment_rng.randrange(len(supplier_ids))
            ]
            purchase_seq += 1
            ledger[key] = ledger.get(key, 0) + quantity
            amount = format_money(round_half_even(quantity * rate, 1000))
            sink.write("purchase_invoice", {
                "invoice_id": f"PI-{purchase_seq:07d}",
                "company_id": company_id,
                "warehouse_id": warehouse_id,
                "supplier_id": supplier_id,
                "date": invoice_date.isoformat(),
                "total": amount,
                "is_replenishment": True,
                "lines": [{
                    "item_id": item_ids[item_index],
                    "quantity": format_quantity(quantity),
                    "rate": format_money(rate),
                    "amount": amount,
                }],
            })

        def _consume(company_id, warehouse_id, item_index, quantity, day):
            """Deduct milli units once the pair can cover them: when stock
            would go negative, the replenishment purchase invoices are
            written first, so the ledger never dips below zero."""
            key = (warehouse_id, item_ids[item_index])
            have = ledger.get(key, 0)
            if quantity > have:
                _replenish(company_id, warehouse_id, item_index, quantity, day)
                have = ledger[key]
            ledger[key] = have - quantity

        def _flush_purchase_return(job, return_day):
            nonlocal pr_seq
            line_index = purchase_return_rng.randrange(len(job["raw"]))
            item_index, original_qty, rate = job["raw"][line_index]
            top = min(original_qty, _TRANSFER_QTY_MAX_MILLI)
            quantity = purchase_return_rng.randrange(1, top + 1)
            _consume(
                job["company_id"], job["warehouse_id"],
                item_index, quantity, return_day,
            )
            pr_seq += 1
            amount = format_money(round_half_even(quantity * rate, 1000))
            sink.write("purchase_return", {
                "purchase_return_id": f"PR-{pr_seq:06d}",
                "company_id": job["company_id"],
                "warehouse_id": job["warehouse_id"],
                "supplier_id": job["supplier_id"],
                "original_invoice_id": job["invoice_id"],
                "date": return_day.isoformat(),
                "total": amount,
                "lines": [{
                    "item_id": item_ids[item_index],
                    "quantity": format_quantity(quantity),
                    "rate": format_money(rate),
                    "amount": amount,
                    "original_line": line_index + 1,
                }],
            })

        def _flush_sales_return(job, return_day):
            nonlocal sr_seq
            line_index = sales_return_rng.randrange(len(job["raw"]))
            item_index, original_qty, rate = job["raw"][line_index]
            top = min(original_qty, _TRANSFER_QTY_MAX_MILLI)
            quantity = sales_return_rng.randrange(1, top + 1)
            key = (job["warehouse_id"], item_ids[item_index])
            ledger[key] = ledger.get(key, 0) + quantity
            sr_seq += 1
            amount = format_money(round_half_even(quantity * rate, 1000))
            sink.write("sales_return", {
                "sales_return_id": f"SR-{sr_seq:06d}",
                "company_id": job["company_id"],
                "warehouse_id": job["warehouse_id"],
                "customer_id": job["customer_id"],
                "original_invoice_id": job["invoice_id"],
                "date": return_day.isoformat(),
                "total": amount,
                "lines": [{
                    "item_id": item_ids[item_index],
                    "quantity": format_quantity(quantity),
                    "rate": format_money(rate),
                    "amount": amount,
                    "original_line": line_index + 1,
                }],
            })

        def _flush_payment(job, payment_day):
            nonlocal payment_seq
            payment_seq += 1
            sink.write("payment", {
                "payment_id": f"PY-{payment_seq:06d}",
                "direction": job["direction"],
                "party_type": job["party_type"],
                "party_id": job["party_id"],
                "date": payment_day.isoformat(),
                "amount": job["amount"],
                "allocations": [{
                    "invoice_id": job["invoice_id"],
                    "amount": job["amount"],
                }],
            })

        period_days = (profile.period_end - profile.period_start).days + 1
        for day_offset in range(period_days):
            day = profile.period_start + dt.timedelta(days=day_offset)
            for _ in range(journals_by_day[day]):
                journal_seq += 1
                sink.write(
                    "journal_entry",
                    _make_entry(journal_rng, company_ids, day, journal_seq),
                )
            for _ in range(purchases_by_day[day]):
                purchase_seq += 1
                purchase_plan_index += 1
                company_id = company_ids[purchase_rng.randrange(len(company_ids))]
                warehouse_id = warehouses_by_company[company_id][
                    purchase_rng.randrange(profile.warehouses_per_company)
                ]
                supplier_id = supplier_ids[purchase_rng.randrange(len(supplier_ids))]
                invoice, raw = _make_invoice(
                    purchase_rng, f"PI-{purchase_seq:07d}", "supplier_id",
                    supplier_id, company_id, warehouse_id, day, item_cum,
                    item_ids, item_rates, line_cum, profile.lines_min,
                )
                invoice["is_replenishment"] = False
                sink.write("purchase_invoice", invoice)
                for item_index, quantity, _rate in raw:
                    key = (warehouse_id, item_ids[item_index])
                    ledger[key] = ledger.get(key, 0) + quantity
                if _designated(
                    purchase_plan_index - 1, profile.purchase_invoices,
                    target_paid_purchases,
                ):
                    delay = profile.delay_min_days + payment_rng.randrange(
                        profile.delay_max_days - profile.delay_min_days + 1
                    )
                    due_buckets.setdefault(day + dt.timedelta(days=delay), []).append({
                        "direction": "payable",
                        "party_type": "supplier",
                        "party_id": supplier_id,
                        "invoice_id": invoice["invoice_id"],
                        "amount": invoice["total"],
                    })
                if _designated(
                    purchase_plan_index - 1, profile.purchase_invoices, target_pr,
                ):
                    delay = _RETURN_DELAY_MIN_DAYS + purchase_return_rng.randrange(
                        _RETURN_DELAY_MAX_DAYS - _RETURN_DELAY_MIN_DAYS + 1
                    )
                    purchase_return_jobs.setdefault(
                        day + dt.timedelta(days=delay), []
                    ).append({
                        "company_id": company_id,
                        "warehouse_id": warehouse_id,
                        "supplier_id": supplier_id,
                        "invoice_id": invoice["invoice_id"],
                        "raw": raw,
                    })
            for _ in range(transfers_by_day[day]):
                company_id = company_ids[transfer_rng.randrange(len(company_ids))]
                warehouses_here = warehouses_by_company[company_id]
                if len(warehouses_here) < 2:
                    continue  # single-warehouse companies hold no transfers
                from_index = transfer_rng.randrange(len(warehouses_here))
                other = len(warehouses_here) - 1
                to_index = (
                    from_index + 1 + transfer_rng.randrange(other)
                ) % len(warehouses_here)
                from_warehouse = warehouses_here[from_index]
                to_warehouse = warehouses_here[to_index]
                item_index = weighted_index(transfer_rng, item_cum)
                quantity = transfer_rng.randrange(
                    1, _TRANSFER_QTY_MAX_MILLI + 1
                )
                _consume(company_id, from_warehouse, item_index, quantity, day)
                transfer_seq += 1
                rate = item_rates[item_index]
                sink.write("stock_transfer", {
                    "transfer_id": f"TR-{transfer_seq:06d}",
                    "company_id": company_id,
                    "date": day.isoformat(),
                    "from_warehouse_id": from_warehouse,
                    "to_warehouse_id": to_warehouse,
                    "item_id": item_ids[item_index],
                    "quantity": format_quantity(quantity),
                    "rate": format_money(rate),
                    "amount": format_money(round_half_even(quantity * rate, 1000)),
                })
            for _ in range(adjustments_by_day[day]):
                company_id = company_ids[adjust_rng.randrange(len(company_ids))]
                warehouse_id = warehouses_by_company[company_id][
                    adjust_rng.randrange(profile.warehouses_per_company)
                ]
                item_index = weighted_index(adjust_rng, item_cum)
                direction = ("in", "out")[adjust_rng.randrange(2)]
                quantity = adjust_rng.randrange(1, _ADJUST_QTY_MAX_MILLI + 1)
                rate = item_rates[item_index]
                amount = format_money(round_half_even(quantity * rate, 1000))
                if direction == "in":
                    ledger_key = (warehouse_id, item_ids[item_index])
                    ledger[ledger_key] = ledger.get(ledger_key, 0) + quantity
                else:
                    _consume(company_id, warehouse_id, item_index, quantity, day)
                reasons = (
                    ADJUSTMENT_REASONS_IN if direction == "in"
                    else ADJUSTMENT_REASONS_OUT
                )
                adjustment_seq += 1
                sink.write("stock_adjustment", {
                    "adjustment_id": f"AD-{adjustment_seq:06d}",
                    "company_id": company_id,
                    "warehouse_id": warehouse_id,
                    "item_id": item_ids[item_index],
                    "date": day.isoformat(),
                    "direction": direction,
                    "reason_code": reasons[adjust_rng.randrange(len(reasons))],
                    "quantity": format_quantity(quantity),
                    "rate": format_money(rate),
                    "amount": amount,
                })
            for _ in range(sales_by_day[day]):
                sales_plan_index += 1
                sales_seq += 1
                company_id = company_ids[sales_rng.randrange(len(company_ids))]
                warehouse_id = warehouses_by_company[company_id][
                    sales_rng.randrange(profile.warehouses_per_company)
                ]
                customer_id = customer_ids[weighted_index(sales_rng, customer_cum)]
                invoice, raw = _make_invoice(
                    sales_rng, f"SI-{sales_seq:07d}", "customer_id", customer_id,
                    company_id, warehouse_id, day, item_cum, item_ids,
                    item_rates, line_cum, profile.lines_min,
                )
                for item_index, quantity, _rate in raw:
                    _consume(company_id, warehouse_id, item_index, quantity, day)
                sink.write("sales_invoice", invoice)
                if _designated(
                    sales_plan_index - 1, profile.sales_invoices, target_paid_sales,
                ):
                    delay = profile.delay_min_days + payment_rng.randrange(
                        profile.delay_max_days - profile.delay_min_days + 1
                    )
                    due_buckets.setdefault(day + dt.timedelta(days=delay), []).append({
                        "direction": "receivable",
                        "party_type": "customer",
                        "party_id": customer_id,
                        "invoice_id": invoice["invoice_id"],
                        "amount": invoice["total"],
                    })
                if _designated(
                    sales_plan_index - 1, profile.sales_invoices, target_sr,
                ):
                    delay = _RETURN_DELAY_MIN_DAYS + sales_return_rng.randrange(
                        _RETURN_DELAY_MAX_DAYS - _RETURN_DELAY_MIN_DAYS + 1
                    )
                    sales_return_jobs.setdefault(
                        day + dt.timedelta(days=delay), []
                    ).append({
                        "company_id": company_id,
                        "warehouse_id": warehouse_id,
                        "customer_id": customer_id,
                        "invoice_id": invoice["invoice_id"],
                        "raw": raw,
                    })
            for job in purchase_return_jobs.pop(day, []):
                _flush_purchase_return(job, day)
            for job in sales_return_jobs.pop(day, []):
                _flush_sales_return(job, day)
            for job in due_buckets.pop(day, []):
                _flush_payment(job, day)
        pending = []
        for due_day, jobs in purchase_return_jobs.items():
            pending.extend((due_day, 0, job) for job in jobs)
        for due_day, jobs in sales_return_jobs.items():
            pending.extend((due_day, 1, job) for job in jobs)
        for due_day, jobs in due_buckets.items():
            pending.extend((due_day, 2, job) for job in jobs)
        pending.sort(key=lambda item: (item[0], item[1]))
        for return_day, kind, job in pending:
            if kind == 0:
                _flush_purchase_return(job, return_day)
            elif kind == 1:
                _flush_sales_return(job, return_day)
            else:
                _flush_payment(job, return_day)
        counts = sink.finish()
    elapsed = time.monotonic() - started
    write_manifest(out_dir / "manifest.json", {
        "generator": GENERATOR_NAME,
        "generator_version": GENERATOR_VERSION,
        "profile": profile.name,
        "profile_sha256": profile.sha256,
        "seed": seed,
        "format": mode,
        "files": counts,
    })
    return {
        "profile": profile.name,
        "profile_sha256": profile.sha256,
        "seed": seed,
        "format": mode,
        "rows": counts,
        "seconds": round(elapsed, 3),
    }