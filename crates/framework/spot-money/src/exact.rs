//! Exact integer arithmetic under the decimals.
//!
//! The decimal type silently rounds (or truncates) when an exact result needs more digits than
//! it can hold. Money must never do that, so every operation here works on `i128` mantissas and
//! reduced fractions with checked arithmetic and turns anything that does not fit into
//! [`MoneyError::Overflow`] or [`MoneyError::Inexact`].

use rust_decimal::Decimal;

use crate::error::{MoneyError, Result};
use crate::rounding::Mode;

/// The most fractional digits a decimal holds.
pub(crate) const MAX_SCALE: u32 = 28;

/// `10^n`, if it fits.
pub(crate) fn pow10(n: u32) -> Option<i128> {
    10_i128.checked_pow(n)
}

fn overflow(operation: &'static str) -> MoneyError {
    MoneyError::Overflow { operation }
}

pub(crate) fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    i128::try_from(a).unwrap_or(1)
}

/// A decimal from a mantissa and a scale, removing trailing zeros first when the mantissa is
/// too wide, and failing (never rounding) when it still does not fit.
pub(crate) fn decimal(
    mut mantissa: i128,
    mut scale: u32,
    operation: &'static str,
) -> Result<Decimal> {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    if scale > MAX_SCALE {
        return Err(MoneyError::Inexact { operation });
    }
    Decimal::try_from_i128_with_scale(mantissa, scale).map_err(|_| overflow(operation))
}

/// Like [`decimal`] but keeps `scale` exactly, widening the mantissa; fails when it does not fit.
pub(crate) fn decimal_at_scale(
    mantissa: i128,
    scale: u32,
    operation: &'static str,
) -> Result<Decimal> {
    if scale > MAX_SCALE {
        return Err(MoneyError::Inexact { operation });
    }
    Decimal::try_from_i128_with_scale(mantissa, scale).map_err(|_| overflow(operation))
}

/// `a ± b` or `a × b` as exact decimals.
pub(crate) fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (ma, mb, scale) = align(a, b, "addition")?;
    let sum = ma.checked_add(mb).ok_or_else(|| overflow("addition"))?;
    decimal_keeping(sum, scale, "addition")
}

pub(crate) fn sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (ma, mb, scale) = align(a, b, "subtraction")?;
    let diff = ma.checked_sub(mb).ok_or_else(|| overflow("subtraction"))?;
    decimal_keeping(diff, scale, "subtraction")
}

/// A decimal at `scale`, falling back to fewer digits only to fit 96 bits.
fn decimal_keeping(mantissa: i128, scale: u32, operation: &'static str) -> Result<Decimal> {
    match Decimal::try_from_i128_with_scale(mantissa, scale) {
        Ok(d) => Ok(d),
        Err(_) => decimal(mantissa, scale, operation),
    }
}

/// Both mantissas at their common (larger) scale.
fn align(a: Decimal, b: Decimal, operation: &'static str) -> Result<(i128, i128, u32)> {
    let scale = a.scale().max(b.scale());
    let up = |d: Decimal| -> Result<i128> {
        let factor = pow10(scale - d.scale()).ok_or_else(|| overflow(operation))?;
        d.mantissa()
            .checked_mul(factor)
            .ok_or_else(|| overflow(operation))
    };
    Ok((up(a)?, up(b)?, scale))
}

/// The exact product. Keeps the natural scale (the sum of the scales, so `1.50 * 2.0` is
/// `3.000`) when it fits, and otherwise drops trailing zeros before giving up.
pub(crate) fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    let natural = a.mantissa().checked_mul(b.mantissa()).and_then(|m| {
        let scale = a.scale() + b.scale();
        (scale <= MAX_SCALE)
            .then(|| Decimal::try_from_i128_with_scale(m, scale).ok())
            .flatten()
    });
    if let Some(d) = natural {
        return Ok(d);
    }
    let (a, b) = (a.normalize(), b.normalize());
    let product = a
        .mantissa()
        .checked_mul(b.mantissa())
        .ok_or_else(|| overflow("multiplication"))?;
    decimal(product, a.scale() + b.scale(), "multiplication")
}

/// An exact fraction `num / den` with `den > 0`, in lowest terms.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frac {
    pub(crate) num: i128,
    pub(crate) den: i128,
}

impl Frac {
    pub(crate) fn new(num: i128, den: i128, operation: &'static str) -> Result<Frac> {
        if den == 0 {
            return Err(overflow(operation));
        }
        let (num, den) = if den < 0 {
            (
                num.checked_neg().ok_or_else(|| overflow(operation))?,
                den.checked_neg().ok_or_else(|| overflow(operation))?,
            )
        } else {
            (num, den)
        };
        let g = gcd(num, den).max(1);
        Ok(Frac {
            num: num / g,
            den: den / g,
        })
    }

    pub(crate) fn of(d: Decimal) -> Result<Frac> {
        let den = pow10(d.scale()).ok_or_else(|| overflow("conversion"))?;
        Frac::new(d.mantissa(), den, "conversion")
    }

    pub(crate) fn mul(self, o: Frac, operation: &'static str) -> Result<Frac> {
        // Cross-reduce first so intermediate products stay small.
        let (a, d) = (gcd(self.num, o.den).max(1), gcd(o.num, self.den).max(1));
        let num = (self.num / a)
            .checked_mul(o.num / d)
            .ok_or_else(|| overflow(operation))?;
        let den = (self.den / d)
            .checked_mul(o.den / a)
            .ok_or_else(|| overflow(operation))?;
        Frac::new(num, den, operation)
    }

    pub(crate) fn div(self, o: Frac, operation: &'static str) -> Result<Frac> {
        if o.num == 0 {
            return Err(overflow(operation));
        }
        self.mul(
            Frac {
                num: o.den,
                den: o.num,
            },
            operation,
        )
    }

    pub(crate) fn add(self, o: Frac, operation: &'static str) -> Result<Frac> {
        let g = gcd(self.den, o.den).max(1);
        let den = (self.den / g)
            .checked_mul(o.den)
            .ok_or_else(|| overflow(operation))?;
        let left = self
            .num
            .checked_mul(o.den / g)
            .ok_or_else(|| overflow(operation))?;
        let right = o
            .num
            .checked_mul(self.den / g)
            .ok_or_else(|| overflow(operation))?;
        Frac::new(
            left.checked_add(right).ok_or_else(|| overflow(operation))?,
            den,
            operation,
        )
    }
}

/// `n / d` rounded to an integer by `mode`; `d` must be positive.
///
/// Works from the floor quotient and the remainder (`0 <= r < d`): `r == 0` is exact; otherwise
/// the answer is the floor or the floor plus one, chosen by the mode. Ties are `2r == d`,
/// compared as `r == d - r` so nothing overflows.
pub(crate) fn round_quotient(n: i128, d: i128, mode: Mode) -> Result<i128> {
    debug_assert!(d > 0);
    let q = n.div_euclid(d);
    let r = n.rem_euclid(d);
    if r == 0 {
        return Ok(q);
    }
    let next = || q.checked_add(1).ok_or_else(|| overflow("rounding"));
    let negative = n < 0;
    match mode {
        Mode::Down => Ok(q),
        Mode::Up => next(),
        Mode::TowardZero => {
            if negative {
                next()
            } else {
                Ok(q)
            }
        }
        Mode::AwayFromZero => {
            if negative {
                Ok(q)
            } else {
                next()
            }
        }
        Mode::HalfUp | Mode::HalfDown | Mode::HalfEven => {
            let below = r < d - r;
            let tie = r == d - r;
            if below {
                Ok(q)
            } else if !tie {
                next()
            } else {
                match mode {
                    Mode::HalfUp => {
                        if negative {
                            Ok(q)
                        } else {
                            next()
                        }
                    }
                    Mode::HalfDown => {
                        if negative {
                            next()
                        } else {
                            Ok(q)
                        }
                    }
                    _ => {
                        if q % 2 == 0 {
                            Ok(q)
                        } else {
                            next()
                        }
                    }
                }
            }
        }
    }
}
