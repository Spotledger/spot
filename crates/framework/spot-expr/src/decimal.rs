//! Decimal arithmetic on (mantissa, scale) pairs, with the rounding rule of specification
//! section 6.3: **half to even**, at the scale of the widest operand (at least 6 for division).
//!
//! Intermediate values are `i128`. An operation whose exact intermediate does not fit is
//! `expr.overflow`, as is a result with more than 28 significant digits. These limits are part
//! of the specification so every implementation fails on the same inputs.

use rust_decimal::Decimal;

use crate::error::{EvalError, EvalResult, codes};

/// The most significant digits a decimal may have.
const MAX_DIGITS: u32 = 28;

pub(crate) fn overflow() -> EvalError {
    EvalError::new(codes::OVERFLOW, "the decimal result is out of range")
}

pub(crate) fn pow10(n: u32) -> Option<i128> {
    10_i128.checked_pow(n)
}

/// The mantissa (signed) and scale of a decimal.
pub(crate) fn parts(d: &Decimal) -> (i128, u32) {
    (d.mantissa(), d.scale())
}

/// A decimal from a mantissa and scale; `expr.overflow` beyond 28 digits.
pub(crate) fn make(mantissa: i128, scale: u32) -> EvalResult<Decimal> {
    let limit = pow10(MAX_DIGITS).ok_or_else(overflow)?;
    if scale > MAX_DIGITS || mantissa.unsigned_abs() >= limit.unsigned_abs() {
        return Err(overflow());
    }
    Ok(Decimal::from_i128_with_scale(mantissa, scale))
}

/// `n / d` rounded half to even. `d` is not zero.
fn div_round(n: i128, d: i128) -> EvalResult<i128> {
    let (n, d) = if d < 0 {
        (
            n.checked_neg().ok_or_else(overflow)?,
            d.checked_neg().ok_or_else(overflow)?,
        )
    } else {
        (n, d)
    };
    let q = n / d;
    let r = n % d;
    if r == 0 {
        return Ok(q);
    }
    let twice = r.unsigned_abs().checked_mul(2).ok_or_else(overflow)?;
    let away = match twice.cmp(&d.unsigned_abs()) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => q % 2 != 0,
        std::cmp::Ordering::Less => false,
    };
    if away {
        let step = if n < 0 { -1 } else { 1 };
        q.checked_add(step).ok_or_else(overflow)
    } else {
        Ok(q)
    }
}

/// Rescales a mantissa upward (exact).
fn up(m: i128, from: u32, to: u32) -> EvalResult<i128> {
    if to <= from {
        return Ok(m);
    }
    m.checked_mul(pow10(to - from).ok_or_else(overflow)?)
        .ok_or_else(overflow)
}

pub(crate) fn add(a: (i128, u32), b: (i128, u32)) -> EvalResult<Decimal> {
    let s = a.1.max(b.1);
    let sum = up(a.0, a.1, s)?
        .checked_add(up(b.0, b.1, s)?)
        .ok_or_else(overflow)?;
    make(sum, s)
}

pub(crate) fn sub(a: (i128, u32), b: (i128, u32)) -> EvalResult<Decimal> {
    let s = a.1.max(b.1);
    let diff = up(a.0, a.1, s)?
        .checked_sub(up(b.0, b.1, s)?)
        .ok_or_else(overflow)?;
    make(diff, s)
}

pub(crate) fn mul(a: (i128, u32), b: (i128, u32)) -> EvalResult<Decimal> {
    let s = a.1.max(b.1);
    let product = a.0.checked_mul(b.0).ok_or_else(overflow)?;
    let exact_scale = a.1 + b.1;
    let diff = exact_scale - s;
    let m = if diff == 0 {
        product
    } else if diff > 38 {
        0
    } else {
        div_round(product, pow10(diff).ok_or_else(overflow)?)?
    };
    make(m, s)
}

/// `a / b` rounded half-even to scale `max(s, 6)`. `b` is not zero.
pub(crate) fn div(a: (i128, u32), b: (i128, u32)) -> EvalResult<Decimal> {
    let t = a.1.max(b.1).max(6);
    let num =
        a.0.checked_mul(pow10(t + b.1).ok_or_else(overflow)?)
            .ok_or_else(overflow)?;
    let den =
        b.0.checked_mul(pow10(a.1).ok_or_else(overflow)?)
            .ok_or_else(overflow)?;
    make(div_round(num, den)?, t)
}

/// `a % b` at scale `s`, with the sign of `a`. `b` is not zero.
pub(crate) fn rem(a: (i128, u32), b: (i128, u32)) -> EvalResult<Decimal> {
    let s = a.1.max(b.1);
    let x = up(a.0, a.1, s)?;
    let y = up(b.0, b.1, s)?;
    make(x.checked_rem(y).ok_or_else(overflow)?, s)
}

/// `d` rounded half-even to exactly `scale` places.
pub(crate) fn round(d: (i128, u32), scale: u32) -> EvalResult<Decimal> {
    if d.1 <= scale {
        make(up(d.0, d.1, scale)?, scale)
    } else {
        make(
            div_round(d.0, pow10(d.1 - scale).ok_or_else(overflow)?)?,
            scale,
        )
    }
}

/// The integer part, truncated toward zero.
pub(crate) fn trunc(d: (i128, u32)) -> i128 {
    match pow10(d.1) {
        Some(p) => d.0 / p,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: i128, s: u32) -> (i128, u32) {
        (m, s)
    }

    fn text(r: EvalResult<Decimal>) -> String {
        r.map(|x| x.to_string())
            .unwrap_or_else(|e| e.code.to_owned())
    }

    #[test]
    fn rounding_is_half_even() {
        // 0.125 -> 0.12 and 0.135 -> 0.14 at scale 2.
        assert_eq!(text(round(d(125, 3), 2)), "0.12");
        assert_eq!(text(round(d(135, 3), 2)), "0.14");
        assert_eq!(text(round(d(-125, 3), 2)), "-0.12");
        assert_eq!(text(round(d(-135, 3), 2)), "-0.14");
        assert_eq!(text(round(d(5, 0), 2)), "5.00");
    }

    #[test]
    fn multiplication_rounds_to_the_widest_scale() {
        assert_eq!(text(mul(d(125, 2), d(125, 2))), "1.56"); // 1.5625
        assert_eq!(text(mul(d(15, 1), d(15, 1))), "2.2"); // 2.25 -> 2.2 (even)
        assert_eq!(text(mul(d(25, 1), d(3, 0))), "7.5");
    }

    #[test]
    fn division_has_at_least_six_places() {
        assert_eq!(text(div(d(1, 0), d(3, 0))), "0.333333");
        assert_eq!(text(div(d(2, 0), d(3, 0))), "0.666667");
        assert_eq!(text(div(d(10, 1), d(4, 0))), "0.250000");
        assert_eq!(text(div(d(-1, 0), d(3, 0))), "-0.333333");
    }

    #[test]
    fn overflow_is_reported() {
        let big = d(9_999_999_999_999_999_999_999_999_999, 0);
        assert_eq!(text(add(big, d(1, 0))), codes::OVERFLOW);
        assert_eq!(text(mul(big, d(10, 0))), codes::OVERFLOW);
    }
}
