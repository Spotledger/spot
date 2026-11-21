//! Exact decoding of PostgreSQL `numeric`.
//!
//! `rust_decimal`'s own `FromSql` silently *rounds* a value that has more digits than it can
//! hold (a product with 28 integer digits and 4 fractional digits comes back without its
//! fraction). A storage layer must never do that, so the binary `numeric` format is decoded here
//! and anything that does not fit exactly is an error: more than 28 fractional digits, a
//! mantissa above 96 bits, `NaN` or an infinity.

use std::error::Error;

use rust_decimal::Decimal;
use tokio_postgres::types::{FromSql, Type, accepts};

/// A `numeric` that fits `rust_decimal` without any loss.
pub(super) struct ExactNumeric(pub(super) Decimal);

const NEGATIVE: u16 = 0x4000;
/// `rust_decimal` keeps a 96-bit mantissa.
const MAX_MANTISSA: u128 = (1 << 96) - 1;

impl<'a> FromSql<'a> for ExactNumeric {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        decode(raw)
            .map(ExactNumeric)
            .ok_or_else(|| "numeric does not fit exactly".into())
    }

    accepts!(NUMERIC);
}

fn be16(raw: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*raw.get(at)?, *raw.get(at + 1)?]))
}

/// Header: digit groups, weight of the first group (in base 10000), sign, display scale; then
/// the groups, most significant first.
fn decode(raw: &[u8]) -> Option<Decimal> {
    let groups = usize::from(be16(raw, 0)?);
    let weight = i32::from(be16(raw, 2)? as i16);
    let sign = be16(raw, 4)?;
    let scale = u32::from(be16(raw, 6)?);
    if sign != 0 && sign != NEGATIVE {
        return None; // NaN, +Infinity, -Infinity
    }
    if scale > 28 || raw.len() != 8 + 2 * groups {
        return None;
    }
    let mut mantissa: u128 = 0;
    for i in 0..groups {
        let digit = u128::from(be16(raw, 8 + 2 * i)?);
        // Group `i` weighs 10000^(weight - i); the mantissa counts units of 10^-scale.
        let exponent = 4 * (i64::from(weight) - i as i64) + i64::from(scale);
        let part = if exponent >= 0 {
            digit.checked_mul(10_u128.checked_pow(u32::try_from(exponent).ok()?)?)?
        } else {
            let divisor = 10_u128.checked_pow(u32::try_from(-exponent).ok()?)?;
            // Digits past the display scale are zero in a well-formed value.
            if digit % divisor != 0 {
                return None;
            }
            digit / divisor
        };
        mantissa = mantissa.checked_add(part)?;
        if mantissa > MAX_MANTISSA {
            return None;
        }
    }
    let signed = i128::try_from(mantissa).ok()?;
    Some(Decimal::from_i128_with_scale(
        if sign == NEGATIVE { -signed } else { signed },
        scale,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `numeric` wire bytes for the given parts.
    fn wire(groups: &[u16], weight: i16, sign: u16, scale: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((groups.len() as u16).to_be_bytes());
        out.extend(weight.to_be_bytes());
        out.extend(sign.to_be_bytes());
        out.extend(scale.to_be_bytes());
        for g in groups {
            out.extend(g.to_be_bytes());
        }
        out
    }

    #[test]
    fn decodes_values_exactly() {
        // 12345.67 = groups [1, 2345, 6700], weight 1, scale 2
        assert_eq!(
            decode(&wire(&[1, 2345, 6700], 1, 0, 2)).unwrap(),
            Decimal::new(1_234_567, 2)
        );
        // -0.0005 = group [5], weight -1 (0.0000..0005 in the first fractional group), scale 4
        assert_eq!(
            decode(&wire(&[5], -1, NEGATIVE, 4)).unwrap(),
            Decimal::new(-5, 4)
        );
        // zero with scale 2 has no groups
        let zero = decode(&wire(&[], 0, 0, 2)).unwrap();
        assert_eq!((zero, zero.scale()), (Decimal::ZERO, 2));
        // 10000 = group [1], weight 1, scale 0
        assert_eq!(
            decode(&wire(&[1], 1, 0, 0)).unwrap(),
            Decimal::new(10_000, 0)
        );
    }

    #[test]
    fn refuses_what_does_not_fit_instead_of_rounding() {
        assert!(decode(&wire(&[], 0, 0xC000, 0)).is_none(), "NaN");
        assert!(decode(&wire(&[1], 0, 0, 29)).is_none(), "scale above 28");
        // 29 integer digits do not fit 96 bits
        assert!(
            decode(&wire(
                &[9, 9999, 9999, 9999, 9999, 9999, 9999, 9999],
                7,
                0,
                0
            ))
            .is_none()
        );
        // a truncated body
        assert!(decode(&wire(&[1], 0, 0, 0)[..9]).is_none());
    }
}
