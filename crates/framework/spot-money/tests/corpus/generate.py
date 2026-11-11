#!/usr/bin/env python3
"""Writes the rounding corpus (*.json next to this file) from first principles.

The expected values come from exact rational arithmetic (`fractions.Fraction`) and the modes as
documented in `src/rounding.rs`; nothing here is taken from any ERP or its source, and nothing
here shares code with the Rust implementation. Run `python3 generate.py` to regenerate; the
files are checked in so the tests need no Python.

`handwritten.json` is NOT generated: its expected values were worked out by hand and serve as a
cross-check of this generator as much as of the crate.
"""
import json
import math
import os
from fractions import Fraction

HERE = os.path.dirname(os.path.abspath(__file__))
MODES = ["half_up", "half_down", "half_even", "up", "down", "toward_zero", "away_from_zero"]


def frac(text):
    return Fraction(text)


def fmt(value, scale):
    """Exact text of a Fraction that has at most `scale` fractional digits."""
    scaled = value * 10**scale
    assert scaled.denominator == 1, (value, scale)
    n = int(scaled)
    sign = "-" if n < 0 else ""
    n = abs(n)
    if scale == 0:
        return f"{sign}{n}"
    digits = str(n).rjust(scale + 1, "0")
    return f"{sign}{digits[:-scale]}.{digits[-scale:]}"


def round_count(r, mode):
    """The integer nearest `r` (a Fraction) under `mode`, from the definitions alone."""
    lo = math.floor(r)
    if r == lo:
        return lo
    hi = lo + 1
    if mode == "down":
        return lo
    if mode == "up":
        return hi
    if mode == "toward_zero":
        return lo if r > 0 else hi
    if mode == "away_from_zero":
        return hi if r > 0 else lo
    # the half modes: distance to each neighbour
    d_lo, d_hi = r - lo, hi - r
    if d_lo < d_hi:
        return lo
    if d_hi < d_lo:
        return hi
    # a tie
    if mode == "half_up":  # away from zero
        return hi if r > 0 else lo
    if mode == "half_down":  # toward zero
        return lo if r > 0 else hi
    if mode == "half_even":
        return lo if lo % 2 == 0 else hi
    raise ValueError(mode)


def do_round(x, scale, mode, step=None):
    unit = frac(step) if step else Fraction(1, 10**scale)
    return round_count(x / unit, mode) * unit


def rule(scale, mode, step=None):
    r = {"scale": scale, "mode": mode}
    if step:
        r["step"] = step
    return r


def allocate(total, weights, scale, mode="half_up", step=None):
    unit = frac(step) if step else Fraction(1, 10**scale)
    units = Fraction(total) / unit
    assert units.denominator == 1
    units = int(units)
    ws = [frac(w) for w in weights]
    if all(w == 0 for w in ws):
        ws = [Fraction(1)] * len(ws)
    s = sum(ws)
    if s == 0:
        return None
    sign = -1 if units < 0 else 1
    mag = abs(units)
    ideal = [Fraction(mag) * w / s for w in ws]
    floors = [math.floor(i) for i in ideal]
    rems = [i - f for i, f in zip(ideal, floors)]
    left = mag - sum(floors)
    order = sorted(range(len(ws)), key=lambda i: (-rems[i], i))
    counts = floors[:]
    for i in order[:left]:
        counts[i] += 1
    parts = [sign * c * unit for c in counts]
    assert sum(parts) == Fraction(total)
    return [fmt(p, scale) for p in parts]


cases = {}


def add(file, case):
    cases.setdefault(file, []).append(case)


# --- every mode at positive, negative and exactly-half values, scales 0 to 6 ------------------
HALVES = ["0.5", "1.5", "2.5", "-0.5", "-1.5", "-2.5", "0.4", "0.6", "-0.4", "-0.6", "7.5", "-7.5", "0.49", "0.51", "0", "3"]
for mode in MODES:
    for scale in [0, 1, 2, 3, 4, 6]:
        for h in HALVES:
            # `h` is in units of 10^-scale: 2.5 at scale 2 is 0.025.
            x = frac(h) / 10**scale
            text = fmt(x, scale + 2) if scale + 2 <= 28 else str(x)
            # strip trailing zeros so values read naturally, keep at least one digit
            text = text.rstrip("0").rstrip(".") if "." in text else text
            want = fmt(do_round(x, scale, mode), scale)
            add("round_modes.json", {"op": "round", "value": text, "rule": rule(scale, mode), "expect": want})

# values with more digits than any scale, large magnitudes, and values already on the grid
for mode in MODES:
    for value in ["123456789012345.675", "-123456789012345.675", "0.0000005", "-0.0000005", "99999.995", "-99999.995", "1.005", "2.675", "1.015", "0.125"]:
        for scale in [2, 6]:
            want = fmt(do_round(frac(value), scale, mode), scale)
            add("round_modes.json", {"op": "round", "value": value, "rule": rule(scale, mode), "expect": want})

# --- cash rounding steps ---------------------------------------------------------------------
for step, scale in [("0.05", 2), ("0.10", 2), ("0.5", 1), ("0.25", 2), ("5", 0), ("0.5", 2), ("0.01", 2), ("0.20", 2), ("1", 0)]:
    for mode in ["half_up", "half_even", "down", "up", "toward_zero"]:
        for value in ["10.02", "10.03", "10.025", "-10.025", "10.075", "-10.02", "0.00", "7.50", "12.5", "-0.026"]:
            want = fmt(do_round(frac(value), scale, mode, step), scale)
            add("round_steps.json", {"op": "round", "value": value, "rule": rule(scale, mode, step), "expect": want})

# --- multiplication, exact then rounded ---------------------------------------------------------
MULS = [("12.50", "3.333", 2), ("0.10", "3", 2), ("19.99", "0.5", 2), ("1.005", "10", 2), ("100.00", "0.333333", 4), ("7", "0.125", 3), ("-4.50", "2.5", 2), ("4.50", "-2.5", 2), ("0.01", "0.5", 2), ("1234567.89", "0.0001", 2), ("0.333", "0.333", 3), ("99.99", "99.99", 2)]
for price, qty, scale in MULS:
    for mode in ["half_up", "half_even", "down", "away_from_zero"]:
        exact = frac(price) * frac(qty)
        add("mul.json", {"op": "mul", "price": price, "currency": "USD", "qty": qty, "rule": rule(scale, mode), "expect": fmt(do_round(exact, scale, mode), scale)})
for price, qty in [("12.50", "3"), ("0.10", "0.1"), ("1.5", "2.5"), ("-3.25", "4")]:
    exact = frac(price) * frac(qty)
    add("mul.json", {"op": "mul_exact", "price": price, "currency": "EUR", "qty": qty, "expect": str(exact.numerator / exact.denominator) if exact.denominator == 1 else fmt(exact, 4)})

# --- percentages and sums -------------------------------------------------------------------------
for amount, rate in [("200.00", "18"), ("99.99", "7.5"), ("0.01", "50"), ("-80.00", "12.5"), ("1000", "0.01"), ("33.33", "33.33")]:
    exact = frac(amount) * frac(rate) / 100
    digits = 8
    add("percent_sum.json", {"op": "percent", "amount": amount, "currency": "USD", "rate": rate, "expect": fmt(exact, digits).rstrip("0").rstrip(".") if "." in fmt(exact, digits) else fmt(exact, digits)})
for values in [["0.1", "0.2"], ["1.10", "2.205", "-0.005"], ["999999999999.99", "0.01"], ["-1", "1"], [], ["0.0000001", "0.0000002", "0.0000003"], ["5"] * 7, ["-0.1", "-0.2", "-0.3"]]:
    total = sum((frac(v) for v in values), Fraction(0))
    text = fmt(total, 8).rstrip("0").rstrip(".") if "." in fmt(total, 8) else fmt(total, 8)
    add("percent_sum.json", {"op": "sum", "currency": "USD", "values": values, "expect": text})

# --- allocation ---------------------------------------------------------------------------------------
ALLOC = [
    ("0.01", ["1", "1", "1"], 2), ("100.00", ["1", "1", "1"], 2), ("10.00", ["1", "2"], 2), ("-0.01", ["1", "1", "1"], 2), ("-100.00", ["1", "1", "1"], 2),
    ("0.02", ["0", "0", "0"], 2), ("0.03", ["0", "1", "1"], 2), ("0.00", ["1", "2", "3"], 2), ("1.00", ["0.5", "0.25", "0.25"], 2), ("100", ["1", "1", "1"], 0),
    ("0.10", ["3", "3", "3", "1"], 2), ("5.00", ["1"], 2), ("0.01", ["1", "1"], 2), ("1000.00", ["12.5", "37.5", "50"], 2), ("19.99", ["0.1", "0.2", "0.3", "0.4"], 2),
    ("0.001", ["1", "1", "1"], 3), ("100.00", ["-1", "2", "2"], 2), ("100.00", ["3", "-1"], 2), ("-30.00", ["1", "2", "3"], 2), ("7.00", ["1", "1", "1", "1", "1", "1", "1", "1"], 2),
    ("0.07", ["1", "1", "1", "1", "1", "1", "1", "1"], 2), ("1.00", ["1e0" if False else "1", "0", "0"], 2), ("123456789.12", ["7", "11", "13"], 2), ("0.05", ["1", "1", "1"], 2),
    ("2.00", ["1", "1", "1"], 1),
]
for total, weights, scale in ALLOC:
    parts = allocate(total, weights, scale)
    if scale == 1:
        total = "2.0"
    add("allocate.json", {"op": "allocate", "total": fmt(frac(total), scale), "currency": "USD", "weights": weights, "rule": rule(scale, "half_up"), "expect": parts})
# on a cash grid the parts are multiples of the step
for total, weights, step in [("1.00", ["1", "1", "1"], "0.05"), ("0.30", ["1", "1", "1"], "0.05"), ("10.00", ["1", "2"], "0.25"), ("-1.00", ["1", "1", "1"], "0.05")]:
    add("allocate.json", {"op": "allocate", "total": total, "currency": "CHF", "weights": weights, "rule": rule(2, "half_up", step), "expect": allocate(total, weights, 2, "half_up", step)})
# errors
add("errors.json", {"op": "allocate", "total": "10.005", "currency": "USD", "weights": ["1", "1"], "rule": rule(2, "half_up"), "expect_error": "money.off_grid"})
add("errors.json", {"op": "allocate", "total": "1.00", "currency": "USD", "weights": [], "rule": rule(2, "half_up"), "expect_error": "money.bad_weights"})
add("errors.json", {"op": "allocate", "total": "1.00", "currency": "USD", "weights": ["1", "-1"], "rule": rule(2, "half_up"), "expect_error": "money.bad_weights"})
add("errors.json", {"op": "allocate", "total": "1.02", "currency": "USD", "weights": ["1", "1"], "rule": rule(2, "half_up", "0.05"), "expect_error": "money.off_grid"})
add("errors.json", {"op": "round", "value": "1", "rule": rule(29, "half_up"), "expect_error": "money.bad_scale"})
add("errors.json", {"op": "round", "value": "1", "rule": rule(2, "half_up", "0"), "expect_error": "money.bad_step"})
add("errors.json", {"op": "round", "value": "1", "rule": rule(2, "half_up", "-0.05"), "expect_error": "money.bad_step"})
add("errors.json", {"op": "round", "value": "1", "rule": rule(1, "half_up", "0.05"), "expect_error": "money.bad_step"})
add("errors.json", {"op": "round", "value": "79228162514264337593543950335", "rule": rule(2, "half_up"), "expect_error": "money.overflow"})
add("errors.json", {"op": "round", "value": "79228162514264337593543950335", "rule": rule(0, "half_up"), "expect": "79228162514264337593543950335"})
add("errors.json", {"op": "sum", "currency": "USD", "values": ["79228162514264337593543950335", "1"], "expect_error": "money.overflow"})
add("errors.json", {"op": "mul_exact", "price": "79228162514264337593543950335", "currency": "USD", "qty": "2", "expect_error": "money.overflow"})
add("errors.json", {"op": "mul_exact", "price": "0.1234567890123456789", "currency": "USD", "qty": "0.1234567890123456789", "expect_error": "money.inexact"})
add("errors.json", {"op": "add_currencies", "left": "USD", "right": "EUR", "expect_error": "money.currency_mismatch"})
add("errors.json", {"op": "tax_incl_to_excl", "gross": "100", "rate": "-100", "rule": rule(2, "half_up"), "expect_error": "money.bad_rate"})
add("errors.json", {"op": "tax_incl_to_excl", "gross": "100", "rate": "-150", "rule": rule(2, "half_up"), "expect_error": "money.bad_rate"})
add("errors.json", {"op": "tax_excl_to_incl", "net": "100", "rate": "-100", "rule": rule(2, "half_up"), "expect_error": "money.bad_rate"})

# --- tax -------------------------------------------------------------------------------------------------
TAX = [("118.00", "18"), ("100.00", "18"), ("100.00", "5"), ("99.99", "7.5"), ("0.01", "18"), ("1234.56", "12.5"), ("-118.00", "18"), ("-100.00", "18"), ("50.00", "0"), ("19.99", "20"), ("1000000.00", "16.5"), ("0.05", "10")]
for amount, rate in TAX:
    for mode in ["half_up", "half_even", "down", "up"]:
        r = rule(2, mode)
        a, t = frac(amount), frac(rate)
        add("tax.json", {"op": "tax_incl_to_excl", "gross": amount, "rate": rate, "rule": r, "expect": fmt(do_round(a / (1 + t / 100), 2, mode), 2)})
        add("tax.json", {"op": "tax_excl_to_incl", "net": amount, "rate": rate, "rule": r, "expect": fmt(do_round(a * (1 + t / 100), 2, mode), 2)})
        add("tax.json", {"op": "tax_on_exclusive", "net": amount, "rate": rate, "rule": r, "expect": fmt(do_round(a * t / 100, 2, mode), 2)})
        net = do_round(a / (1 + t / 100), 2, mode)
        add("tax.json", {"op": "tax_in_inclusive", "gross": amount, "rate": rate, "rule": r, "expect": fmt(a - net, 2)})
# per-line against on-total over several lines
LINES = [(["0.10", "0.10", "0.10"], "5"), (["19.99", "5.01", "0.35"], "18"), (["33.33", "33.33", "33.34"], "7.5"), (["100.00", "-40.00", "20.00"], "12"), (["0.01", "0.01"], "50"), (["12.35", "7.65", "1.10", "0.05"], "16.5")]
for amounts, rate in LINES:
    for mode in ["half_up", "half_even"]:
        for scope in ["per_line", "on_total"]:
            a = [frac(x) for x in amounts]
            t = frac(rate)
            if scope == "per_line":
                lines = [do_round(x * t / 100, 2, mode) for x in a]
                total = sum(lines)
                shares = [fmt(x, 2) for x in lines]
            else:
                total = do_round(sum(a) * t / 100, 2, mode)
                shares = allocate(fmt(total, 2), amounts, 2, mode)
            add("tax.json", {"op": "tax_lines_exclusive", "amounts": amounts, "rate": rate, "rule": rule(2, mode), "scope": scope, "expect": {"total": fmt(total, 2), "lines": shares}})
INCL = [(["118.00", "59.00"], "18"), (["10.00", "10.00", "10.00"], "5"), (["99.99", "0.01"], "7.5")]
for amounts, rate in INCL:
    for scope in ["per_line", "on_total"]:
        a = [frac(x) for x in amounts]
        t = frac(rate)
        mode = "half_up"
        def tax_in(g):
            return g - do_round(g / (1 + t / 100), 2, mode)
        if scope == "per_line":
            lines = [tax_in(x) for x in a]
            total = sum(lines)
            shares = [fmt(x, 2) for x in lines]
        else:
            total = tax_in(sum(a))
            shares = allocate(fmt(total, 2), amounts, 2, mode)
        add("tax.json", {"op": "tax_lines_inclusive", "amounts": amounts, "rate": rate, "rule": rule(2, mode), "scope": scope, "expect": {"total": fmt(total, 2), "lines": shares}})

for name, items in cases.items():
    with open(os.path.join(HERE, name), "w") as out:
        # One case per line: readable diffs and no file longer than the review gate allows.
        out.write("[\n")
        out.write(",\n".join(json.dumps(item, separators=(",", ":")) for item in items))
        out.write("\n]\n")
    print(f"{name}: {len(items)} cases")
print("total:", sum(len(v) for v in cases.values()))
