"""Integer money and quantity arithmetic.

ADR-12: no floating-point arithmetic for money or quantities anywhere. Money
is carried as integer minor units (paisa) and quantities as integer
milli-units (thousandths of a unit). Both are written as fixed-scale decimal
strings: money with 2 decimals, quantities with 3 decimals. Rounding is
half-even, done in exact integer arithmetic.
"""

MONEY_SCALE = 2
QUANTITY_SCALE = 3
MONEY_DENOMINATOR = 100
QUANTITY_DENOMINATOR = 1000


class MoneyError(ValueError):
    """Raised when a value is not a well-formed fixed-scale decimal string."""


def round_half_even(numerator: int, denominator: int) -> int:
    """Divide numerator by denominator and round halves to even.

    Exact integer arithmetic; inputs must be non-negative (money and
    quantities are never negative in this model).
    """
    if numerator < 0 or denominator <= 0:
        raise ValueError("need a non-negative numerator and a positive denominator")
    quotient, remainder = divmod(numerator, denominator)
    twice = 2 * remainder
    if twice > denominator or (twice == denominator and quotient % 2 == 1):
        quotient += 1
    return quotient


def format_money(paisa: int) -> str:
    """Render integer paisa as a fixed-scale decimal string with 2 decimals."""
    if paisa < 0:
        raise ValueError("money amounts are never negative in this model")
    return f"{paisa // MONEY_DENOMINATOR}.{paisa % MONEY_DENOMINATOR:02d}"


def format_quantity(milli: int) -> str:
    """Render integer milli-units as a fixed-scale decimal string with 3 decimals."""
    if milli < 0:
        raise ValueError("quantities are never negative in this model")
    return f"{milli // QUANTITY_DENOMINATOR}.{milli % QUANTITY_DENOMINATOR:03d}"


def parse_fixed(text: str, scale: int) -> int:
    """Parse a fixed-scale decimal string ("12.34" with scale 2) into integer
    minor units. Anything else raises MoneyError."""
    if not isinstance(text, str) or text.count(".") != 1:
        raise MoneyError(f"expected a fixed-scale decimal string, got {text!r}")
    whole, frac = text.split(".")
    if not whole.isdigit() or not frac.isdigit() or len(frac) != scale:
        raise MoneyError(f"expected a decimal string with {scale} decimals, got {text!r}")
    return int(whole) * 10**scale + int(frac)