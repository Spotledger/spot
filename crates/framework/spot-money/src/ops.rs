//! Operations on amounts: multiplication, percentages and allocation.

use rust_decimal::Decimal;

use crate::amount::{Money, Quantity};
use crate::error::{MoneyError, Result};
use crate::exact::{self, gcd};
use crate::rounding::RoundingRule;

/// `price * quantity`, exact, in the price's currency. The result keeps every digit (scale up to
/// 28); round it with the rule the caller names, or use [`mul`].
pub fn mul_exact(price: &Money, quantity: &Quantity) -> Result<Money> {
    Ok(price.with_value(exact::mul(price.value(), quantity.value())?))
}

/// `price * quantity`, exact, then rounded by `rule`: the rule is the caller's choice (the
/// `line_total` rule of the document, say), never implied.
pub fn mul(price: &Money, quantity: &Quantity, rule: &RoundingRule) -> Result<Money> {
    mul_exact(price, quantity)?.rounded(rule)
}

/// `amount * rate / 100`, exact (`rate` is a percentage: `18` for 18%). The result keeps every
/// digit; `money.inexact` if it needs more than 28 fractional digits.
pub fn percent(amount: &Money, rate: Decimal) -> Result<Money> {
    let product = exact::mul(amount.value(), rate)?;
    let value = exact::decimal(product.mantissa(), product.scale() + 2, "percentage")?;
    Ok(amount.with_value(value))
}

/// `amount * rate / 100` rounded by `rule`.
pub fn percent_rounded(amount: &Money, rate: Decimal, rule: &RoundingRule) -> Result<Money> {
    percent(amount, rate)?.rounded(rule)
}

/// Splits `total` in proportion to `weights` so that the parts sum to `total` **exactly**, each
/// part a whole number of the rule's units (`10^-scale`, or the step).
///
/// This is the largest-remainder method on exact integers:
///
/// 1. `total` must lie on the rule's grid (a multiple of its unit), else `money.off_grid`: the
///    parts could not sum to it.
/// 2. Every part gets the floor of its ideal share; the units left over (fewer than the number
///    of parts) go one each to the parts with the largest remainders. **Equal remainders are
///    broken by position: the earlier part wins.**
/// 3. A negative total is the mirror of its absolute value: it is allocated as `-allocate(-total)`,
///    so a credit note splits exactly like its invoice (the extra unit goes to the same parts,
///    each negated).
///
/// **Weights.** They are decimals of any scale; only their ratios matter. A zero weight gets a
/// zero part (unless every weight is zero, see below). A negative weight gets a part of the
/// opposite sign to the total, in proportion: shares are `total * weight / sum(weights)` with signed
/// weights, so the parts still sum to the total. If every weight is zero the total is split
/// **equally** (as if all weights were 1). If the weights sum to zero but are not all zero
/// (`+1` and `-1`) nothing can be apportioned: `money.bad_weights`. No weights at all is also
/// `money.bad_weights`.
pub fn allocate(total: &Money, weights: &[Decimal], rule: &RoundingRule) -> Result<Vec<Money>> {
    let parts = allocate_decimals(total.value(), weights, rule).map_err(|e| match e {
        // Name the amount, with its currency, rather than the bare number.
        MoneyError::OffGrid(_) => MoneyError::OffGrid(total.to_string()),
        other => other,
    })?;
    Ok(parts
        .into_iter()
        .map(|part| total.with_value(part))
        .collect())
}

/// [`allocate`] on bare decimals (the rules and errors are the same).
pub fn allocate_decimals(
    total: Decimal,
    weights: &[Decimal],
    rule: &RoundingRule,
) -> Result<Vec<Decimal>> {
    let overflow = || MoneyError::Overflow {
        operation: "allocation",
    };
    if weights.is_empty() {
        return Err(MoneyError::BadWeights(
            "there are no weights to allocate to",
        ));
    }
    let units = rule
        .unit_count(total)?
        .ok_or_else(|| MoneyError::OffGrid(total.to_string()))?;
    let negative = units < 0;
    let units = units.checked_abs().ok_or_else(overflow)?;

    // Weights as integers at a common scale; only ratios matter, so reduce by their gcd.
    let scale = weights.iter().map(Decimal::scale).max().unwrap_or(0);
    let mut ints = Vec::with_capacity(weights.len());
    for w in weights {
        let factor = exact::pow10(scale - w.scale()).ok_or_else(overflow)?;
        ints.push(w.mantissa().checked_mul(factor).ok_or_else(overflow)?);
    }
    if ints.iter().all(|w| *w == 0) {
        ints.iter_mut().for_each(|w| *w = 1);
    }
    let g = ints.iter().fold(0, |g, w| gcd(g, *w)).max(1);
    ints.iter_mut().for_each(|w| *w /= g);
    let mut sum = ints
        .iter()
        .try_fold(0_i128, |s, w| s.checked_add(*w))
        .ok_or_else(overflow)?;
    if sum == 0 {
        return Err(MoneyError::BadWeights(
            "the weights sum to zero, so nothing can be apportioned",
        ));
    }
    if sum < 0 {
        ints.iter_mut().for_each(|w| *w = -*w);
        sum = -sum;
    }

    let mut parts: Vec<(i128, i128)> = Vec::with_capacity(ints.len()); // (floor, remainder)
    for w in &ints {
        let share = units.checked_mul(*w).ok_or_else(overflow)?;
        parts.push((share.div_euclid(sum), share.rem_euclid(sum)));
    }
    let floors = parts
        .iter()
        .try_fold(0_i128, |s, (f, _)| s.checked_add(*f))
        .ok_or_else(overflow)?;
    let leftover = units.checked_sub(floors).ok_or_else(overflow)?;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    // Largest remainder first; `sort_by` is stable, so equal remainders keep their positions.
    order.sort_by(|a, b| parts[*b].1.cmp(&parts[*a].1));
    let mut counts: Vec<i128> = parts.iter().map(|(f, _)| *f).collect();
    for index in order
        .into_iter()
        .take(usize::try_from(leftover).map_err(|_| overflow())?)
    {
        counts[index] += 1;
    }
    counts
        .into_iter()
        .map(|c| {
            let signed = if negative {
                c.checked_neg().ok_or_else(overflow)?
            } else {
                c
            };
            rule.units_to_decimal(signed)
        })
        .collect()
}
