"""Profile loading and validation.

A profile is a TOML file with three tables: [period], [counts] and
[distributions]. Everything is validated here so generation and
verification can rely on the values. Booleans are rejected wherever an
integer is required (type(x) is int, since bool is an int subclass).

Per-mille shares cap at 1000; opening stock needs min <= max milli. The
new v2 counts (journal entries, stock transfers and stock adjustments) can
be zero; the loader never couples them to the invoice counts.
"""

import datetime as dt
import hashlib
import tomllib
from dataclasses import dataclass
from pathlib import Path

SHIPPED_DIR = Path(__file__).resolve().parent.parent / "profiles"


class ProfileError(ValueError):
    """Raised when a profile is missing, malformed or inconsistent."""


@dataclass(frozen=True)
class Profile:
    name: str
    sha256: str
    period_start: dt.date
    period_end: dt.date
    companies: int
    warehouses_per_company: int
    items: int
    customers: int
    suppliers: int
    sales_invoices: int
    purchase_invoices: int
    payments: int
    journal_entries: int
    stock_transfers: int
    stock_adjustments: int
    lines_min: int
    lines_peak: int
    lines_max: int
    item_popularity_skew: int
    customer_activity_skew: int
    paid_share_per_mille: int
    sales_return_per_mille: int
    purchase_return_per_mille: int
    opening_stock_coverage_per_mille: int
    opening_stock_min_milli: int
    opening_stock_max_milli: int
    opening_receivable_per_mille: int
    opening_payable_per_mille: int
    monthly_seasonality: tuple
    delay_min_days: int
    delay_max_days: int


def resolve_profile_path(spec: str) -> Path:
    """Accept a path to a TOML file, or the name of a shipped profile."""
    candidate = Path(spec)
    if candidate.is_file():
        return candidate
    shipped = SHIPPED_DIR / f"{spec}.toml"
    if shipped.is_file():
        return shipped
    raise ProfileError(f"profile not found: {spec!r} is neither a file nor a shipped profile name")


def shipped_profiles() -> list:
    """Sorted paths of the shipped profile files."""
    return sorted(SHIPPED_DIR.glob("*.toml"))


def _table(data, key, path) -> dict:
    value = data.get(key)
    if not isinstance(value, dict):
        raise ProfileError(f"{path.name}: missing [{key}] table")
    return value


def _date(table, key, path) -> dt.date:
    value = table.get(key)
    if not isinstance(value, str):
        raise ProfileError(f"{path.name}: period.{key} must be an ISO date string")
    try:
        return dt.date.fromisoformat(value)
    except ValueError as exc:
        raise ProfileError(f"{path.name}: period.{key}: expected an ISO date, got {value!r}") from exc


def _count(table, key, minimum, path) -> int:
    value = table.get(key)
    if type(value) is not int or value < minimum:
        raise ProfileError(f"{path.name}: {key} must be an integer >= {minimum}")
    return value


def _permille(table, key, path) -> int:
    """A count that means parts per thousand, so capped at 1000."""
    value = _count(table, key, 0, path)
    if value > 1000:
        raise ProfileError(f"{path.name}: {key} must be at most 1000")
    return value


def load_profile(spec: str) -> Profile:
    path = resolve_profile_path(spec)
    raw = path.read_bytes()
    try:
        data = tomllib.loads(raw.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as exc:
        raise ProfileError(f"{path.name}: invalid TOML: {exc}") from exc

    period = _table(data, "period", path)
    counts = _table(data, "counts", path)
    dist = _table(data, "distributions", path)
    lines = _table(dist, "lines_per_invoice", path)
    delay = _table(dist, "payment_delay", path)

    start = _date(period, "start", path)
    end = _date(period, "end", path)
    if start > end:
        raise ProfileError(f"{path.name}: period.start {start} is after period.end {end}")

    companies = _count(counts, "companies", 1, path)
    warehouses_per_company = _count(counts, "warehouses_per_company", 1, path)
    items = _count(counts, "items", 1, path)
    customers = _count(counts, "customers", 1, path)
    suppliers = _count(counts, "suppliers", 1, path)
    sales = _count(counts, "sales_invoices", 0, path)
    purchase = _count(counts, "purchase_invoices", 0, path)
    payments = _count(counts, "payments", 0, path)
    journal_entries = _count(counts, "journal_entries", 0, path)
    stock_transfers = _count(counts, "stock_transfers", 0, path)
    stock_adjustments = _count(counts, "stock_adjustments", 0, path)

    lines_min = _count(lines, "min", 1, path)
    lines_peak = _count(lines, "peak", 1, path)
    lines_max = _count(lines, "max", 1, path)
    if not lines_min <= lines_peak <= lines_max:
        raise ProfileError(f"{path.name}: distributions.lines_per_invoice needs min <= peak <= max")

    item_skew = _count(dist, "item_popularity_skew", 1, path)
    customer_skew = _count(dist, "customer_activity_skew", 1, path)
    if item_skew > 8 or customer_skew > 8:
        raise ProfileError(f"{path.name}: skew exponents above 8 are not supported")

    paid_share = _permille(dist, "paid_share_per_mille", path)
    sales_return_share = _permille(dist, "sales_return_per_mille", path)
    purchase_return_share = _permille(dist, "purchase_return_per_mille", path)
    stock_coverage = _permille(dist, "opening_stock_coverage_per_mille", path)
    stock_min = _count(dist, "opening_stock_min_milli", 1, path)
    stock_max = _count(dist, "opening_stock_max_milli", 1, path)
    if stock_min > stock_max:
        raise ProfileError(
            f"{path.name}: distributions needs opening_stock_min_milli <= opening_stock_max_milli"
        )
    receivable_share = _permille(dist, "opening_receivable_per_mille", path)
    payable_share = _permille(dist, "opening_payable_per_mille", path)

    expected_payments = sales * paid_share // 1000 + purchase * paid_share // 1000
    if payments != expected_payments:
        raise ProfileError(
            f"{path.name}: counts.payments must be {expected_payments}: every payment fully pays "
            f"exactly one invoice, and paid_share_per_mille designates {paid_share} per mille of "
            f"the {sales} sales and {purchase} purchase invoices"
        )

    seasonality = dist.get("monthly_seasonality")
    if not isinstance(seasonality, list) or len(seasonality) != 12 or any(
        type(weight) is not int or weight < 0 for weight in seasonality
    ):
        raise ProfileError(f"{path.name}: monthly_seasonality must be a list of 12 non-negative integers")
    if sum(seasonality) < 1:
        raise ProfileError(f"{path.name}: monthly_seasonality must have a positive sum")

    delay_min = _count(delay, "min_days", 0, path)
    delay_max = _count(delay, "max_days", 0, path)
    if delay_min > delay_max:
        raise ProfileError(f"{path.name}: payment_delay needs min_days <= max_days")
    if delay_max > 365:
        raise ProfileError(f"{path.name}: payment_delay.max_days must be at most 365")

    return Profile(
        name=path.stem,
        sha256=hashlib.sha256(raw).hexdigest(),
        period_start=start,
        period_end=end,
        companies=companies,
        warehouses_per_company=warehouses_per_company,
        items=items,
        customers=customers,
        suppliers=suppliers,
        sales_invoices=sales,
        purchase_invoices=purchase,
        payments=payments,
        journal_entries=journal_entries,
        stock_transfers=stock_transfers,
        stock_adjustments=stock_adjustments,
        lines_min=lines_min,
        lines_peak=lines_peak,
        lines_max=lines_max,
        item_popularity_skew=item_skew,
        customer_activity_skew=customer_skew,
        paid_share_per_mille=paid_share,
        sales_return_per_mille=sales_return_share,
        purchase_return_per_mille=purchase_return_share,
        opening_stock_coverage_per_mille=stock_coverage,
        opening_stock_min_milli=stock_min,
        opening_stock_max_milli=stock_max,
        opening_receivable_per_mille=receivable_share,
        opening_payable_per_mille=payable_share,
        monthly_seasonality=tuple(seasonality),
        delay_min_days=delay_min,
        delay_max_days=delay_max,
    )