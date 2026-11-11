//! Tax rounding as pure functions on decimals.
//!
//! Rates are percentages (`18` for 18%). Every function takes the [`RoundingRule`] to round with
//! and does its arithmetic exactly, so for example `100 / 1.18` is rounded once, from the exact
//! fraction, never from a rounded intermediate.
//!
//! # Rounding order (`RoundingScope`)
//!
//! Tax on a document with several lines can be rounded two ways, and the ERP decides per tax rule:
//!
//! - [`RoundingScope::PerLine`]: round each line's tax, then sum. The total is the sum of the
//!   rounded line taxes.
//! - [`RoundingScope::OnTotal`]: sum the lines first, round once. The total is the rounded tax of
//!   the summed lines, and it is then **allocated back to the lines** by [`crate::allocate`]
//!   (weights = the line amounts, largest remainder, ties to the earlier line), so the line taxes
//!   still add up to the total exactly.
//!
//! The two can differ by a few units of the smallest step: that difference is the reason the
//! choice is data, not code.

use rust_decimal::Decimal;

use crate::amount::sum_decimals;
use crate::error::{MoneyError, Result};
use crate::exact::{self, Frac};
use crate::ops;
use crate::rounding::RoundingRule;

/// Whether tax is rounded per line (then summed) or once on the sum (then allocated back).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoundingScope {
    /// Round each line, then sum.
    PerLine,
    /// Sum the lines, then round once.
    OnTotal,
}

/// The tax of a set of lines: the total, and each line's share of it. The line shares always sum to
/// the total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaxLines {
    /// The tax of the whole document.
    pub total: Decimal,
    /// The tax of each line, in the order given.
    pub lines: Vec<Decimal>,
}

/// `1 + rate/100` as a fraction; the rate must keep it positive.
fn factor(rate: Decimal) -> Result<Frac> {
    let hundred = Frac::of(Decimal::ONE_HUNDRED)?;
    let factor = Frac::of(rate)?.add(hundred, "tax")?.div(hundred, "tax")?;
    if factor.num <= 0 {
        return Err(MoneyError::BadRate(
            "a tax rate of -100% or below leaves no base to compute from",
        ));
    }
    Ok(factor)
}

/// The tax on a tax-exclusive amount: `round(net * rate / 100)`.
pub fn tax_on_exclusive(net: Decimal, rate: Decimal, rule: &RoundingRule) -> Result<Decimal> {
    let part = Frac::of(rate)?.div(Frac::of(Decimal::ONE_HUNDRED)?, "tax")?;
    rule.apply_fraction(Frac::of(net)?.mul(part, "tax")?)
}

/// Adds tax to a tax-exclusive amount: `round(net * (1 + rate/100))`.
pub fn tax_exclusive_to_inclusive(
    net: Decimal,
    rate: Decimal,
    rule: &RoundingRule,
) -> Result<Decimal> {
    rule.apply_fraction(Frac::of(net)?.mul(factor(rate)?, "tax")?)
}

/// Takes tax out of a tax-inclusive amount: `round(gross / (1 + rate/100))`.
pub fn tax_inclusive_to_exclusive(
    gross: Decimal,
    rate: Decimal,
    rule: &RoundingRule,
) -> Result<Decimal> {
    rule.apply_fraction(Frac::of(gross)?.div(factor(rate)?, "tax")?)
}

/// The tax contained in a tax-inclusive amount: `gross - tax_inclusive_to_exclusive(gross)`.
pub fn tax_in_inclusive(gross: Decimal, rate: Decimal, rule: &RoundingRule) -> Result<Decimal> {
    exact::sub(gross, tax_inclusive_to_exclusive(gross, rate, rule)?)
}

/// The tax on tax-exclusive line amounts, rounded by `scope`.
pub fn exclusive_lines(
    nets: &[Decimal],
    rate: Decimal,
    rule: &RoundingRule,
    scope: RoundingScope,
) -> Result<TaxLines> {
    match scope {
        RoundingScope::PerLine => {
            let lines = nets
                .iter()
                .map(|net| tax_on_exclusive(*net, rate, rule))
                .collect::<Result<Vec<_>>>()?;
            Ok(TaxLines {
                total: sum_decimals(lines.iter().copied())?,
                lines,
            })
        }
        RoundingScope::OnTotal => {
            let total = tax_on_exclusive(sum_decimals(nets.iter().copied())?, rate, rule)?;
            spread(total, nets, rule)
        }
    }
}

/// The tax contained in tax-inclusive line amounts, rounded by `scope`. Under
/// [`RoundingScope::OnTotal`] the lines must lie on the rule's grid (they are amounts already
/// rounded), else `money.off_grid`.
pub fn inclusive_lines(
    grosses: &[Decimal],
    rate: Decimal,
    rule: &RoundingRule,
    scope: RoundingScope,
) -> Result<TaxLines> {
    match scope {
        RoundingScope::PerLine => {
            let lines = grosses
                .iter()
                .map(|gross| tax_in_inclusive(*gross, rate, rule))
                .collect::<Result<Vec<_>>>()?;
            Ok(TaxLines {
                total: sum_decimals(lines.iter().copied())?,
                lines,
            })
        }
        RoundingScope::OnTotal => {
            let gross = sum_decimals(grosses.iter().copied())?;
            let total = tax_in_inclusive(gross, rate, rule)?;
            spread(total, grosses, rule)
        }
    }
}

/// Shares `total` among lines in proportion to their amounts, so the shares sum to it.
fn spread(total: Decimal, amounts: &[Decimal], rule: &RoundingRule) -> Result<TaxLines> {
    if amounts.is_empty() {
        return Ok(TaxLines {
            total,
            lines: Vec::new(),
        });
    }
    Ok(TaxLines {
        total,
        lines: ops::allocate_decimals(total, amounts, rule)?,
    })
}
