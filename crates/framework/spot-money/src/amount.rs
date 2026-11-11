//! [`Money`] and [`Quantity`]: two types over the same exact decimal, kept apart by the type
//! system so that a quantity cannot be added to money, and by their unit so that two currencies
//! (or two units of measure) are never mixed.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::currency::{Currency, Uom};
use crate::error::{MoneyError, Result};
use crate::exact;
use crate::rounding::{RoundingRule, decimal_text};

/// An amount of money in one currency. Its value has whatever scale it has; rounding to a rule is
/// explicit ([`Money::rounded`]), never implied by an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    #[serde(with = "decimal_text")]
    value: Decimal,
    currency: Currency,
}

impl Money {
    /// Money of `value` in `currency`.
    pub fn new(value: Decimal, currency: Currency) -> Money {
        Money { value, currency }
    }

    /// Zero in `currency`.
    pub fn zero(currency: Currency) -> Money {
        Money::new(Decimal::ZERO, currency)
    }

    /// The decimal value.
    pub fn value(&self) -> Decimal {
        self.value
    }

    /// The currency.
    pub fn currency(&self) -> &Currency {
        &self.currency
    }

    /// Whether the value is zero.
    pub fn is_zero(&self) -> bool {
        self.value.is_zero()
    }

    /// The same currency with another value.
    pub fn with_value(&self, value: Decimal) -> Money {
        Money {
            value,
            currency: self.currency,
        }
    }

    fn same_currency(&self, other: &Money) -> Result<()> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch {
                left: self.currency.code().to_owned(),
                right: other.currency.code().to_owned(),
            })
        }
    }

    /// `self + other`, exact. A different currency is `money.currency_mismatch`; a result that does
    /// not fit is `money.overflow`.
    pub fn checked_add(&self, other: &Money) -> Result<Money> {
        self.same_currency(other)?;
        Ok(self.with_value(exact::add(self.value, other.value)?))
    }

    /// `self - other`, exact; errors as [`Money::checked_add`].
    pub fn checked_sub(&self, other: &Money) -> Result<Money> {
        self.same_currency(other)?;
        Ok(self.with_value(exact::sub(self.value, other.value)?))
    }

    /// The negated amount.
    pub fn neg(&self) -> Money {
        self.with_value(-self.value)
    }

    /// The amount rounded by `rule`.
    pub fn rounded(&self, rule: &RoundingRule) -> Result<Money> {
        Ok(self.with_value(rule.apply(self.value)?))
    }

    /// The exact sum of `items`, all of which must be in `currency` (`money.currency_mismatch`
    /// otherwise). The result does not depend on the order of `items`, and neither does whether it
    /// fails: see [`sum_decimals`].
    pub fn sum<'a>(
        currency: &Currency,
        items: impl IntoIterator<Item = &'a Money>,
    ) -> Result<Money> {
        let mut values = Vec::new();
        for item in items {
            Money::zero(*currency).same_currency(item)?;
            values.push(item.value);
        }
        Ok(Money::new(sum_decimals(values)?, *currency))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.value, self.currency)
    }
}

/// A quantity, optionally in a unit of measure.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Quantity {
    #[serde(with = "decimal_text")]
    value: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uom: Option<Uom>,
}

impl Quantity {
    /// A quantity of `value` in `uom` (`None` for a bare number).
    pub fn new(value: Decimal, uom: Option<Uom>) -> Quantity {
        Quantity { value, uom }
    }

    /// The decimal value.
    pub fn value(&self) -> Decimal {
        self.value
    }

    /// The unit, if any.
    pub fn uom(&self) -> Option<&Uom> {
        self.uom.as_ref()
    }

    /// The same unit with another value.
    pub fn with_value(&self, value: Decimal) -> Quantity {
        Quantity {
            value,
            uom: self.uom.clone(),
        }
    }

    fn same_uom(&self, other: &Quantity) -> Result<()> {
        if self.uom == other.uom {
            return Ok(());
        }
        let name = |u: &Option<Uom>| {
            u.as_ref()
                .map_or("none".to_owned(), |u| u.as_str().to_owned())
        };
        Err(MoneyError::UomMismatch {
            left: name(&self.uom),
            right: name(&other.uom),
        })
    }

    /// `self + other`, exact. Different units are `money.uom_mismatch`.
    pub fn checked_add(&self, other: &Quantity) -> Result<Quantity> {
        self.same_uom(other)?;
        Ok(self.with_value(exact::add(self.value, other.value)?))
    }

    /// `self - other`, exact. Different units are `money.uom_mismatch`.
    pub fn checked_sub(&self, other: &Quantity) -> Result<Quantity> {
        self.same_uom(other)?;
        Ok(self.with_value(exact::sub(self.value, other.value)?))
    }

    /// The quantity rounded by `rule`.
    pub fn rounded(&self, rule: &RoundingRule) -> Result<Quantity> {
        Ok(self.with_value(rule.apply(self.value)?))
    }

    /// The exact sum of `items`, all in `uom`; order-independent like [`Money::sum`].
    pub fn sum<'a>(
        uom: Option<&Uom>,
        items: impl IntoIterator<Item = &'a Quantity>,
    ) -> Result<Quantity> {
        let mut values = Vec::new();
        let zero = Quantity::new(Decimal::ZERO, uom.cloned());
        for item in items {
            zero.same_uom(item)?;
            values.push(item.value);
        }
        Ok(zero.with_value(sum_decimals(values)?))
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.uom {
            Some(u) => write!(f, "{} {u}", self.value),
            None => write!(f, "{}", self.value),
        }
    }
}

/// The exact sum of decimals.
///
/// The result, and whether the sum fails, depend only on the multiset of values, never on their
/// order: every value is brought to the largest scale, positives and negatives are accumulated
/// apart (each partial sum only grows, so overflow cannot depend on order) and subtracted at the
/// end. An empty sum is zero.
pub fn sum_decimals(values: impl IntoIterator<Item = Decimal>) -> Result<Decimal> {
    let values: Vec<Decimal> = values.into_iter().collect();
    let scale = values.iter().map(Decimal::scale).max().unwrap_or(0);
    let overflow = || MoneyError::Overflow { operation: "sum" };
    let (mut positive, mut negative) = (0_i128, 0_i128);
    for value in &values {
        let factor = exact::pow10(scale - value.scale()).ok_or_else(overflow)?;
        let widened = value.mantissa().checked_mul(factor).ok_or_else(overflow)?;
        if widened >= 0 {
            positive = positive.checked_add(widened).ok_or_else(overflow)?;
        } else {
            negative = negative
                .checked_add(widened.checked_neg().ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
        }
    }
    let total = positive.checked_sub(negative).ok_or_else(overflow)?;
    exact::decimal(total, scale, "sum").and_then(|d| keep_scale(d, total, scale))
}

/// `d` at `scale` when that fits, so a sum of two-digit amounts keeps its two digits.
fn keep_scale(d: Decimal, total: i128, scale: u32) -> Result<Decimal> {
    Ok(Decimal::try_from_i128_with_scale(total, scale).unwrap_or(d))
}
