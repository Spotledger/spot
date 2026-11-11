//! Properties over random inputs: allocation sums exactly, rounding is idempotent and stays within
//! one unit, sums do not depend on order, and nothing panics (including at the limits of the decimal
//! type).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use rust_decimal::{Decimal, RoundingStrategy};
use spot_money::{
    Currency, Mode, Money, Quantity, RoundingRule, RoundingScope, allocate, exclusive_lines,
    inclusive_lines, mul, mul_exact, percent, sum_decimals, tax_exclusive_to_inclusive,
    tax_inclusive_to_exclusive,
};

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}

/// A moderate decimal: a 64-bit mantissa at a scale of 0 to 10.
fn decimal() -> impl Strategy<Value = Decimal> {
    (any::<i64>(), 0_u32..=10).prop_map(|(m, s)| Decimal::new(m, s))
}

/// Any decimal the type can hold, including the extremes.
fn any_decimal() -> impl Strategy<Value = Decimal> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<bool>(),
        0_u32..=28,
    )
        .prop_map(|(lo, mid, hi, neg, scale)| Decimal::from_parts(lo, mid, hi, neg, scale))
}

fn mode() -> impl Strategy<Value = Mode> {
    prop::sample::select(Mode::ALL.to_vec())
}

/// A valid rule: a scale of 0 to 8, a mode and, half the time, a step that fits the scale.
fn rule() -> impl Strategy<Value = RoundingRule> {
    (0_u32..=8, mode(), prop::option::of(1_i64..=25)).prop_map(|(scale, mode, step)| {
        // A step of `n` units of the last digit: always positive and within the scale.
        let step = step.map(|n| Decimal::new(n, scale));
        RoundingRule::new(scale, mode, step).unwrap()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    /// The parts of an allocation sum to the total exactly, each lies on the grid, a negative total
    /// is the mirror of the positive one, and only weights that cannot apportion fail.
    #[test]
    fn allocation_sums_to_the_total(
        units in -1_000_000_000_i64..=1_000_000_000,
        scale in 0_u32..=6,
        weights in prop::collection::vec((-1_000_i32..=1_000, 0_u32..=4), 1..9),
        mode in mode(),
        step in prop::option::of(1_i64..=25),
    ) {
        let weights: Vec<Decimal> = weights.iter().map(|(m, s)| Decimal::new(i64::from(*m), *s)).collect();
        let rule = RoundingRule::new(scale, mode, step.map(|n| Decimal::new(n, scale))).unwrap();
        // A total on the rule's grid.
        let total = Money::new(rule.unit() * Decimal::from(units), usd());
        let sum_weights = sum_decimals(weights.iter().copied()).unwrap();
        let all_zero = weights.iter().all(Decimal::is_zero);
        match allocate(&total, &weights, &rule) {
            Ok(parts) => {
                prop_assert_eq!(parts.len(), weights.len());
                let sum = sum_decimals(parts.iter().map(Money::value)).unwrap();
                prop_assert_eq!(sum, total.value());
                for part in &parts {
                    let count = part.value() / rule.unit();
                    prop_assert!(count.fract().is_zero(), "{} is not on the grid of {}", part, rule);
                    prop_assert_eq!(part.value().scale(), scale);
                }
                // The mirror.
                let mirrored = allocate(&total.neg(), &weights, &rule).unwrap();
                for (a, b) in parts.iter().zip(&mirrored) {
                    prop_assert_eq!(a.value(), -b.value());
                }
            }
            Err(e) => {
                prop_assert_eq!(e.code(), "money.bad_weights");
                prop_assert!(sum_weights.is_zero() && !all_zero, "unexpected failure: {e}");
            }
        }
    }

    /// Rounding twice changes nothing, the result is on the grid and has exactly the scale.
    #[test]
    fn rounding_is_idempotent(value in decimal(), rule in rule()) {
        let once = rule.apply(value).unwrap();
        prop_assert_eq!(rule.apply(once).unwrap(), once);
        prop_assert_eq!(once.scale(), rule.scale());
        prop_assert!((once / rule.unit()).fract().is_zero());
    }

    /// The result is within one unit of the input, and within half a unit for the nearest modes.
    #[test]
    fn rounding_stays_within_one_unit(value in decimal(), rule in rule()) {
        let rounded = rule.apply(value).unwrap();
        let distance = (rounded - value).abs();
        prop_assert!(distance <= rule.unit(), "{value} -> {rounded} under {rule}");
        if matches!(rule.mode(), Mode::HalfUp | Mode::HalfDown | Mode::HalfEven) {
            prop_assert!(distance * Decimal::TWO <= rule.unit(), "{value} -> {rounded} under {rule}");
        }
        // And in the direction the mode promises.
        match rule.mode() {
            Mode::Up => prop_assert!(rounded >= value),
            Mode::Down => prop_assert!(rounded <= value),
            Mode::TowardZero => prop_assert!(rounded.abs() <= value.abs()),
            Mode::AwayFromZero => prop_assert!(rounded.abs() >= value.abs()),
            _ => {}
        }
    }

    /// Without a step the result equals the decimal type's own rounding (an independent
    /// implementation of the same definitions).
    #[test]
    fn rounding_agrees_with_the_decimal_library(value in decimal(), scale in 0_u32..=8, mode in mode()) {
        let strategy = match mode {
            Mode::HalfUp => RoundingStrategy::MidpointAwayFromZero,
            Mode::HalfDown => RoundingStrategy::MidpointTowardZero,
            Mode::HalfEven => RoundingStrategy::MidpointNearestEven,
            Mode::Up => RoundingStrategy::ToPositiveInfinity,
            Mode::Down => RoundingStrategy::ToNegativeInfinity,
            Mode::TowardZero => RoundingStrategy::ToZero,
            Mode::AwayFromZero => RoundingStrategy::AwayFromZero,
        };
        let rule = RoundingRule::at_scale(scale, mode).unwrap();
        prop_assert_eq!(rule.apply(value).unwrap(), value.round_dp_with_strategy(scale, strategy));
    }

    /// A sum, and whether it fails, depend only on the values, not on their order.
    #[test]
    fn sums_do_not_depend_on_order(values in prop::collection::vec(any_decimal(), 0..12), seed in any::<u64>()) {
        let forward = sum_decimals(values.iter().copied());
        let mut reversed = values.clone();
        reversed.reverse();
        let mut shuffled = values.clone();
        let mut state = seed | 1;
        for i in (1..shuffled.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            shuffled.swap(i, (state % (i as u64 + 1)) as usize);
        }
        prop_assert_eq!(&forward, &sum_decimals(reversed));
        prop_assert_eq!(&forward, &sum_decimals(shuffled));
    }

    /// Nothing panics, whatever the inputs, including the limits of the decimal type; every
    /// failure is a typed error with a code.
    #[test]
    fn no_operation_panics(a in any_decimal(), b in any_decimal(), rule in rule(), weights in prop::collection::vec(any_decimal(), 0..5)) {
        let money = Money::new(a, usd());
        let quantity = Quantity::new(b, None);
        let results: Vec<Result<(), spot_money::MoneyError>> = vec![
            rule.apply(a).map(drop),
            money.checked_add(&Money::new(b, usd())).map(drop),
            money.checked_sub(&Money::new(b, usd())).map(drop),
            mul_exact(&money, &quantity).map(drop),
            mul(&money, &quantity, &rule).map(drop),
            percent(&money, b).map(drop),
            allocate(&money, &weights, &rule).map(drop),
            sum_decimals([a, b]).map(drop),
            tax_exclusive_to_inclusive(a, b, &rule).map(drop),
            tax_inclusive_to_exclusive(a, b, &rule).map(drop),
            exclusive_lines(&weights, b, &rule, RoundingScope::PerLine).map(drop),
            exclusive_lines(&weights, b, &rule, RoundingScope::OnTotal).map(drop),
            inclusive_lines(&weights, b, &rule, RoundingScope::OnTotal).map(drop),
        ];
        for result in results {
            if let Err(e) = result {
                prop_assert!(e.code().starts_with("money."));
            }
        }
    }

    /// Per-line and on-total tax each have line shares that sum to their total.
    #[test]
    fn tax_lines_sum_to_their_total(
        cents in prop::collection::vec(-1_000_000_i64..=1_000_000, 1..8),
        rate in (0_u32..=5_000).prop_map(|n| Decimal::new(i64::from(n), 2)),
    ) {
        let rule = RoundingRule::at_scale(2, Mode::HalfUp).unwrap();
        let lines: Vec<Decimal> = cents.iter().map(|c| Decimal::new(*c, 2)).collect();
        for scope in [RoundingScope::PerLine, RoundingScope::OnTotal] {
            for tax in [exclusive_lines(&lines, rate, &rule, scope), inclusive_lines(&lines, rate, &rule, scope)] {
                match tax {
                    Ok(t) => prop_assert_eq!(sum_decimals(t.lines.iter().copied()).unwrap(), t.total),
                    // Lines that sum to zero without being all zero cannot be apportioned.
                    Err(e) => prop_assert_eq!(e.code(), "money.bad_weights"),
                }
            }
        }
    }
}
