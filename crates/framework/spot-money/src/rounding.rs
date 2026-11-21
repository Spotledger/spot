//! Rounding rules as data (ADR-12): a [`RoundingRule`] is a scale, a mode and an optional step.
//!
//! # The modes
//!
//! Ties are values exactly halfway between two candidates. Five modes ignore the sign (they are
//! symmetric: `round(-x) == -round(x)`), two are directional on the number line:
//!
//! | Mode | Rule | `2.5` | `-2.5` | `2.4` | `-2.4` |
//! | --- | --- | --- | --- | --- | --- |
//! | `half_up` | nearest; ties away from zero | 3 | -3 | 2 | -2 |
//! | `half_down` | nearest; ties toward zero | 2 | -2 | 2 | -2 |
//! | `half_even` | nearest; ties to the even candidate | 2 | -2 | 2 | -2 |
//! | `toward_zero` | truncate | 2 | -2 | 2 | -2 |
//! | `away_from_zero` | the next candidate away from zero | 3 | -3 | 3 | -3 |
//! | `up` | toward positive infinity (ceiling) | 3 | -2 | 3 | -2 |
//! | `down` | toward negative infinity (floor) | 2 | -3 | 2 | -3 |
//!
//! (The columns show rounding to a whole number.) `half_up` is the commercial rule; it is
//! symmetric on purpose, so a credit note rounds to exactly the negative of its invoice.
//!
//! # Scale and step
//!
//! `scale` is the number of fractional digits of the result (0 to 28); the result always has
//! exactly that many. `step` makes the candidates multiples of the step instead of multiples of
//! `10^-scale` (cash rounding to 0.05, or to 5 whole units). A step must be positive and have no
//! more fractional digits than `scale`, so every candidate is representable at that scale.
//!
//! Rounding is exact: it works on integers, never on binary fractions, and an input that does not
//! fit is an error, not a wrong answer.

use std::fmt;

use rust_decimal::Decimal;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{MoneyError, Result};
use crate::exact::{self, Frac, MAX_SCALE};

/// How a value is rounded; see the [module documentation](self) for the exact meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Nearest; ties away from zero.
    HalfUp,
    /// Nearest; ties toward zero.
    HalfDown,
    /// Nearest; ties to the even candidate.
    HalfEven,
    /// Toward positive infinity.
    Up,
    /// Toward negative infinity.
    Down,
    /// Truncate.
    TowardZero,
    /// The next candidate away from zero.
    AwayFromZero,
}

impl Mode {
    /// The name used in TOML and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::HalfUp => "half_up",
            Mode::HalfDown => "half_down",
            Mode::HalfEven => "half_even",
            Mode::Up => "up",
            Mode::Down => "down",
            Mode::TowardZero => "toward_zero",
            Mode::AwayFromZero => "away_from_zero",
        }
    }

    /// Reads a mode from its name.
    pub fn parse(name: &str) -> Option<Mode> {
        Some(match name {
            "half_up" => Mode::HalfUp,
            "half_down" => Mode::HalfDown,
            "half_even" => Mode::HalfEven,
            "up" => Mode::Up,
            "down" => Mode::Down,
            "toward_zero" => Mode::TowardZero,
            "away_from_zero" => Mode::AwayFromZero,
            _ => return None,
        })
    }

    /// Every mode.
    pub const ALL: [Mode; 7] = [
        Mode::HalfUp,
        Mode::HalfDown,
        Mode::HalfEven,
        Mode::Up,
        Mode::Down,
        Mode::TowardZero,
        Mode::AwayFromZero,
    ];
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A rounding rule: scale, mode and an optional step. Construct with [`RoundingRule::new`] or by
/// deserializing, both of which validate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RoundingRule {
    scale: u32,
    mode: Mode,
    step: Option<Decimal>,
}

impl RoundingRule {
    /// Validates and builds a rule: `scale` at most 28; `step`, if given, positive with no more
    /// fractional digits than `scale` (trailing zeros do not count).
    pub fn new(scale: u32, mode: Mode, step: Option<Decimal>) -> Result<RoundingRule> {
        if scale > MAX_SCALE {
            return Err(MoneyError::BadScale(scale));
        }
        let step = match step {
            None => None,
            Some(step) => {
                if step <= Decimal::ZERO {
                    return Err(MoneyError::BadStep(format!("step {step} is not positive")));
                }
                let step = step.normalize();
                if step.scale() > scale {
                    return Err(MoneyError::BadStep(format!(
                        "step {step} has more fractional digits than the scale {scale}"
                    )));
                }
                Some(step)
            }
        };
        Ok(RoundingRule { scale, mode, step })
    }

    /// A rule without a step.
    pub fn at_scale(scale: u32, mode: Mode) -> Result<RoundingRule> {
        RoundingRule::new(scale, mode, None)
    }

    /// The number of fractional digits of a rounded value.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The step, if any (normalised: no trailing zeros).
    pub fn step(&self) -> Option<Decimal> {
        self.step
    }

    /// The distance between candidates: the step, or `10^-scale`.
    pub fn unit(&self) -> Decimal {
        self.step.unwrap_or_else(|| Decimal::new(1, self.scale))
    }

    /// Rounds `value`. The result has exactly [`RoundingRule::scale`] fractional digits.
    pub fn apply(&self, value: Decimal) -> Result<Decimal> {
        self.apply_fraction(Frac::of(value)?)
    }

    /// Rounds an exact fraction to this rule's grid.
    pub(crate) fn apply_fraction(&self, value: Frac) -> Result<Decimal> {
        let unit = self.unit();
        let ratio = value.div(Frac::of(unit)?, "rounding")?;
        let count = exact::round_quotient(ratio.num, ratio.den, self.mode)?;
        self.units_to_decimal(count)
    }

    /// The value `count * unit`, at this rule's scale.
    pub(crate) fn units_to_decimal(&self, count: i128) -> Result<Decimal> {
        let unit = self.unit();
        let widened = count
            .checked_mul(unit.mantissa())
            .ok_or(MoneyError::Overflow {
                operation: "rounding",
            })?;
        let up = exact::pow10(self.scale - unit.scale()).ok_or(MoneyError::Overflow {
            operation: "rounding",
        })?;
        let mantissa = widened.checked_mul(up).ok_or(MoneyError::Overflow {
            operation: "rounding",
        })?;
        exact::decimal_at_scale(mantissa, self.scale, "rounding")
    }

    /// How many units of this rule's grid `value` is, if it is exactly on the grid.
    pub(crate) fn unit_count(&self, value: Decimal) -> Result<Option<i128>> {
        let ratio = Frac::of(value)?.div(Frac::of(self.unit())?, "allocation")?;
        Ok((ratio.den == 1).then_some(ratio.num))
    }
}

impl fmt::Display for RoundingRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scale {}, {}", self.scale, self.mode)?;
        if let Some(step) = self.step {
            write!(f, ", step {step}")?;
        }
        Ok(())
    }
}

/// `RoundingRule` as written in TOML or JSON: `{ scale, mode, step }`, the step as a string.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    scale: u32,
    mode: Mode,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal"
    )]
    step: Option<Decimal>,
}

impl Serialize for RoundingRule {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        Wire {
            scale: self.scale,
            mode: self.mode,
            step: self.step,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RoundingRule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        RoundingRule::new(wire.scale, wire.mode, wire.step).map_err(de::Error::custom)
    }
}

/// Decimals in serialized form are strings (`"0.05"`) or whole numbers, never binary fractions.
pub(crate) mod decimal_text {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &Decimal,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Decimal, D::Error> {
        deserializer.deserialize_any(DecimalVisitor)
    }
}

mod optional_decimal {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match value {
            Some(d) => serializer.collect_str(d),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Decimal>, D::Error> {
        decimal_text::deserialize(deserializer).map(Some)
    }
}

struct DecimalVisitor;

impl Visitor<'_> for DecimalVisitor {
    type Value = Decimal;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a decimal written as a string, for example '0.05', or as a whole number")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Decimal, E> {
        parse_decimal(v).map_err(E::custom)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Decimal, E> {
        Ok(Decimal::from(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Decimal, E> {
        Ok(Decimal::from(v))
    }
}

/// Reads a decimal exactly: no silent rounding of digits beyond 28 places, no exponent, no
/// surrounding whitespace.
pub fn parse_decimal(text: &str) -> Result<Decimal> {
    let bad = || MoneyError::BadDecimal(text.to_owned());
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    let well_formed = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1
        && !digits.starts_with('.')
        && !digits.ends_with('.');
    if !well_formed {
        return Err(bad());
    }
    Decimal::from_str_exact(text).map_err(|_| bad())
}
