//! Exact decimal quantities: a signed integer mantissa and a decimal scale, never a float
//! (ADR-12). The same shape `spot-money` uses, kept local so the lineage layer needs no
//! arithmetic of its own beyond exact multiplication and addition.

use std::fmt;

use spot_storage_contract::Value;

use crate::error::LineageError;

/// The largest scale a quantity may carry.
pub const MAX_SCALE: u32 = 18;

/// An exact decimal quantity: `mantissa × 10^−scale`.
///
/// Values are normalised (trailing zeros removed), so two quantities are equal exactly when
/// they denote the same number. Multiplication and addition are exact and report
/// [`LineageError::QuantityOverflow`] rather than rounding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Quantity {
    mantissa: i128,
    scale: u32,
}

impl Quantity {
    /// A quantity of `mantissa × 10^−scale`, normalised.
    #[must_use]
    pub fn new(mantissa: i128, scale: u32) -> Self {
        let mut quantity = Self { mantissa, scale };
        quantity.normalise();
        quantity
    }

    /// One.
    #[must_use]
    pub fn one() -> Self {
        Self::new(1, 0)
    }

    /// Zero.
    #[must_use]
    pub fn zero() -> Self {
        Self::new(0, 0)
    }

    /// A whole number.
    #[must_use]
    pub fn from_int(value: i64) -> Self {
        Self::new(i128::from(value), 0)
    }

    /// The signed integer mantissa.
    #[must_use]
    pub fn mantissa(self) -> i128 {
        self.mantissa
    }

    /// The number of digits after the decimal point.
    #[must_use]
    pub fn scale(self) -> u32 {
        self.scale
    }

    /// Whether the value is zero.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    /// Parses a plain decimal string such as `12`, `-3.5` or `0.125`.
    ///
    /// # Errors
    ///
    /// [`LineageError::BadQuantity`] for anything else: an empty string, a stray character, more
    /// than [`MAX_SCALE`] fractional digits, or a value that does not fit an `i128` mantissa.
    pub fn parse(text: &str) -> Result<Self, LineageError> {
        let bad = || LineageError::BadQuantity {
            text: text.to_owned(),
        };
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        if digits.is_empty() {
            return Err(bad());
        }
        let (int_part, frac_part) = match digits.split_once('.') {
            Some((int_part, frac_part)) => {
                if int_part.is_empty() || frac_part.is_empty() {
                    return Err(bad());
                }
                (int_part, frac_part)
            }
            None => (digits, ""),
        };
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let scale = u32::try_from(frac_part.len()).map_err(|_| bad())?;
        if scale > MAX_SCALE {
            return Err(bad());
        }
        let mut mantissa: i128 = 0;
        for digit in int_part.bytes().chain(frac_part.bytes()) {
            let digit = i128::from(digit - b'0');
            mantissa = mantissa
                .checked_mul(10)
                .and_then(|value| value.checked_add(digit))
                .ok_or_else(bad)?;
        }
        if negative {
            mantissa = -mantissa;
        }
        Ok(Self::new(mantissa, scale))
    }

    /// `self × other`, exact.
    ///
    /// # Errors
    ///
    /// [`LineageError::QuantityOverflow`] when the mantissa or the scale does not fit.
    pub fn checked_mul(self, other: Self) -> Result<Self, LineageError> {
        let mantissa = self
            .mantissa
            .checked_mul(other.mantissa)
            .ok_or(LineageError::QuantityOverflow)?;
        let scale = self
            .scale
            .checked_add(other.scale)
            .ok_or(LineageError::QuantityOverflow)?;
        if scale > MAX_SCALE {
            return Err(LineageError::QuantityOverflow);
        }
        Ok(Self::new(mantissa, scale))
    }

    /// `self + other`, exact.
    ///
    /// # Errors
    ///
    /// [`LineageError::QuantityOverflow`] when the result does not fit.
    pub fn checked_add(self, other: Self) -> Result<Self, LineageError> {
        let scale = self.scale.max(other.scale);
        let left = scale_up(self.mantissa, scale - self.scale)?;
        let right = scale_up(other.mantissa, scale - other.scale)?;
        let mantissa = left
            .checked_add(right)
            .ok_or(LineageError::QuantityOverflow)?;
        Ok(Self::new(mantissa, scale))
    }

    /// Reads a quantity from a stored value: an integer, an exact decimal or a decimal string.
    ///
    /// # Errors
    ///
    /// [`LineageError::BadQuantity`] for a value of any other type or a malformed decimal string.
    pub fn from_value(value: &Value) -> Result<Self, LineageError> {
        match value {
            Value::Int(value) => Ok(Self::from_int(*value)),
            Value::Decimal(value) => Self::parse(&value.to_string()),
            Value::Text(text) => Self::parse(text),
            Value::Null => Err(LineageError::BadQuantity {
                text: "a null value".to_owned(),
            }),
            other => Err(LineageError::BadQuantity {
                text: format!("a value of type {}", type_name(other)),
            }),
        }
    }

    fn normalise(&mut self) {
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale -= 1;
        }
        if self.mantissa == 0 {
            self.scale = 0;
        }
    }
}

fn scale_up(mantissa: i128, power: u32) -> Result<i128, LineageError> {
    let mut value = mantissa;
    for _ in 0..power {
        value = value
            .checked_mul(10)
            .ok_or(LineageError::QuantityOverflow)?;
    }
    Ok(value)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Decimal(_) => "decimal",
        Value::Text(_) => "text",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::DateTime(_) => "datetime",
        Value::Id(_) => "id",
        Value::Json(_) => "json",
        Value::Bytes(_) => "bytes",
        Value::GeoPoint(_) => "geo point",
        Value::Range(_) => "range",
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.mantissa < 0;
        let digits = self.mantissa.unsigned_abs().to_string();
        if negative {
            f.write_str("-")?;
        }
        if self.scale == 0 {
            return f.write_str(&digits);
        }
        let scale = self.scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
        } else {
            digits
        };
        let split = padded.len() - scale;
        write!(f, "{}.{}", &padded[..split], &padded[split..])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::Quantity;
    use crate::error::LineageError;

    #[test]
    fn parses_and_displays_plain_decimals() {
        assert_eq!(Quantity::parse("12").unwrap().to_string(), "12");
        assert_eq!(Quantity::parse("0.125").unwrap().to_string(), "0.125");
        assert_eq!(Quantity::parse("-3.50").unwrap().to_string(), "-3.5");
        assert_eq!(Quantity::parse("0.000").unwrap().to_string(), "0");
        assert_eq!(Quantity::parse("-0").unwrap(), Quantity::zero());
    }

    #[test]
    fn rejects_anything_that_is_not_a_plain_decimal() {
        for text in ["", ".", "1.", ".5", "1.2.3", "1e3", " 1", "abc", "1,5"] {
            let error = Quantity::parse(text).unwrap_err();
            assert_eq!(error.code(), "lineage.bad_quantity", "for {text:?}");
        }
    }

    #[test]
    fn multiplies_and_adds_exactly() {
        let value = Quantity::parse("1.25").unwrap();
        assert_eq!(
            value.checked_mul(Quantity::parse("0.5").unwrap()).unwrap(),
            Quantity::parse("0.625").unwrap()
        );
        assert_eq!(
            value.checked_add(Quantity::parse("0.75").unwrap()).unwrap(),
            Quantity::parse("2").unwrap()
        );
        // 0.1 + 0.2 is exactly 0.3, unlike binary floating point.
        assert_eq!(
            Quantity::parse("0.1")
                .unwrap()
                .checked_add(Quantity::parse("0.2").unwrap())
                .unwrap(),
            Quantity::parse("0.3").unwrap()
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_wrong_number() {
        let huge = Quantity::new(i128::MAX, 0);
        assert!(matches!(
            huge.checked_mul(Quantity::from_int(2)),
            Err(LineageError::QuantityOverflow)
        ));
    }

    #[test]
    fn reads_quantities_from_stored_values() {
        use spot_storage_contract::Value;
        assert_eq!(
            Quantity::from_value(&Value::Int(7)).unwrap(),
            Quantity::from_int(7)
        );
        assert_eq!(
            Quantity::from_value(&Value::Text("2.5".into())).unwrap(),
            Quantity::parse("2.5").unwrap()
        );
        assert_eq!(
            Quantity::from_value(&Value::Null).unwrap_err().code(),
            "lineage.bad_quantity"
        );
    }
}
