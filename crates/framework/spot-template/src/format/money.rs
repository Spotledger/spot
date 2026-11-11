//! The `money` and `number` renderers.

use minijinja::Value;

use crate::error::TemplateError;
use crate::format::{CurrencyInfo, LocaleInfo, group_digits, split_amount};

/// The highest fraction digit count `number` accepts.
const MAX_NUMBER_DECIMALS: u32 = 20;

/// Renders an exact amount as money text: grouped per the locale's pattern,
/// padded to the currency's minor units, with the currency symbol placed per
/// its table entry (`Rs 1,234.56`, `$1,234.56`, `₹12,34,567.89`).
pub(crate) fn money(
    value: &Value,
    currency: &CurrencyInfo,
    locale: &LocaleInfo,
) -> Result<String, TemplateError> {
    let digits = split_amount(value, "money")?;
    // minor_units is a small table constant; u32 → usize is lossless here.
    let minor_units = currency.minor_units as usize;
    if digits.frac.len() > minor_units {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "amount '{}' has {} fractional digits but {} has {} minor units; \
                 money is rounded upstream, templates only print (ADR-12)",
                digits.raw,
                digits.frac.len(),
                currency.code,
                currency.minor_units
            ),
        });
    }
    let mut fraction = digits.frac.clone();
    while fraction.len() < minor_units {
        fraction.push('0');
    }
    let grouped = group_digits(&digits.int_part, locale.pattern, locale.group);
    let number = if minor_units == 0 {
        grouped
    } else {
        format!("{grouped}{}{fraction}", locale.decimal)
    };
    Ok(compose(digits.negative, currency, &number))
}

/// Renders an exact amount or integer as a grouped number with exactly
/// `decimals` fraction digits (padded, never rounded).
pub(crate) fn number(
    value: &Value,
    decimals: u32,
    locale: &LocaleInfo,
) -> Result<String, TemplateError> {
    if decimals > MAX_NUMBER_DECIMALS {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "number expects at most {MAX_NUMBER_DECIMALS} decimals, got {decimals}"
            ),
        });
    }
    let digits = split_amount(value, "number")?;
    if digits.frac.len() > decimals as usize {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "amount '{}' has {} fractional digits, more than the {} \
                 requested 'number' decimals; round upstream (ADR-12)",
                digits.raw,
                digits.frac.len(),
                decimals
            ),
        });
    }
    let mut fraction = digits.frac.clone();
    while fraction.len() < decimals as usize {
        fraction.push('0');
    }
    let grouped = group_digits(&digits.int_part, locale.pattern, locale.group);
    let number = if decimals == 0 {
        grouped
    } else {
        format!("{grouped}{}{fraction}", locale.decimal)
    };
    if digits.negative {
        Ok(format!("-{number}"))
    } else {
        Ok(number)
    }
}

fn compose(negative: bool, currency: &CurrencyInfo, number: &str) -> String {
    let sign = if negative { "-" } else { "" };
    let symbol = currency.symbol;
    if currency.prefix {
        if currency.space {
            format!("{sign}{symbol} {number}")
        } else {
            format!("{sign}{symbol}{number}")
        }
    } else if currency.space {
        format!("{sign}{number} {symbol}")
    } else {
        format!("{sign}{number}{symbol}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{currency, locale};
    use crate::value::DecimalObject;
    use rust_decimal::Decimal;

    fn amount(text: &str) -> Value {
        Value::from_object(DecimalObject::new(
            text.parse::<Decimal>()
                .unwrap_or_else(|error| panic!("bad decimal: {error}")),
        ))
    }

    fn cur(code: &str) -> &'static CurrencyInfo {
        currency(code).unwrap_or_else(|error| panic!("table: {error}"))
    }

    fn en() -> &'static LocaleInfo {
        locale("en").unwrap_or_else(|error| panic!("table: {error}"))
    }

    #[test]
    fn every_registered_currency_formats() {
        for code in ["PKR", "USD", "EUR", "GBP", "AED", "SAR", "CNY", "INR"] {
            let currency = cur(code);
            let text = money(&amount("-1000.5"), currency, en()).unwrap_or_else(|e| panic!("{e}"));
            let expected = if currency.space {
                format!("-{} 1,000.50", currency.symbol)
            } else {
                format!("-{}1,000.50", currency.symbol)
            };
            assert_eq!(text, expected, "{code}");
        }
    }

    #[test]
    fn very_large_values_keep_every_digit() {
        let text = money(&amount("12345678901234567.89"), cur("PKR"), en())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(text, "Rs 12,345,678,901,234,567.89");
    }

    #[test]
    fn pkr_in_en() {
        assert_eq!(
            money(&amount("1234.56"), cur("PKR"), en()).unwrap_or_else(|e| panic!("{e}")),
            "Rs 1,234.56"
        );
    }

    #[test]
    fn ur_pk_grouping_matches_this_task() {
        let info = locale("ur-PK").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            money(&amount("1234.56"), cur("PKR"), info).unwrap_or_else(|e| panic!("{e}")),
            "Rs 1,234.56"
        );
    }

    #[test]
    fn usd_is_adjacent_and_padded() {
        assert_eq!(
            money(&amount("1234.56"), cur("USD"), en()).unwrap_or_else(|e| panic!("{e}")),
            "$1,234.56"
        );
        assert_eq!(
            money(&amount("10.5"), cur("PKR"), en()).unwrap_or_else(|e| panic!("{e}")),
            "Rs 10.50"
        );
    }

    #[test]
    fn zero_and_negative() {
        assert_eq!(
            money(&amount("0"), cur("PKR"), en()).unwrap_or_else(|e| panic!("{e}")),
            "Rs 0.00"
        );
        assert_eq!(
            money(&amount("-987.65"), cur("PKR"), en()).unwrap_or_else(|e| panic!("{e}")),
            "-Rs 987.65"
        );
    }

    #[test]
    fn inr_lakh_grouping() {
        let info = locale("en-IN").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            money(&amount("1234567.89"), cur("INR"), info).unwrap_or_else(|e| panic!("{e}")),
            "₹12,34,567.89"
        );
    }

    #[test]
    fn too_many_fraction_digits_is_an_error() {
        match money(&amount("123.456"), cur("PKR"), en()) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("rounded upstream"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn number_fixed_decimals() {
        assert_eq!(
            number(&amount("1234.56"), 2, en()).unwrap_or_else(|e| panic!("{e}")),
            "1,234.56"
        );
        assert_eq!(
            number(&amount("1234.56"), 4, en()).unwrap_or_else(|e| panic!("{e}")),
            "1,234.5600"
        );
        assert_eq!(
            number(&Value::from(1_234_567_i64), 0, en()).unwrap_or_else(|e| panic!("{e}")),
            "1,234,567"
        );
        assert_eq!(
            number(&amount("-1234.5"), 2, en()).unwrap_or_else(|e| panic!("{e}")),
            "-1,234.50"
        );
    }

    #[test]
    fn number_rejects_extra_fraction_digits() {
        match number(&amount("1234.567"), 2, en()) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("round upstream"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn number_rejects_huge_decimal_counts() {
        match number(&amount("1"), 21, en()) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("20"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
}
