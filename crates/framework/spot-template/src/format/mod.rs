//! Locale and currency tables for the formatting filters, plus the exact
//! digit splitting and grouping shared by them.
//!
//! Only the task's locale and currency set ships in WP-PLT-12; unknown codes
//! are errors, never silent defaults. `ur-PK` ships Latin digits with
//! Western grouping (per the task); Arabic-Indic digits are a later option.

pub(crate) mod dates;
pub(crate) mod money;
pub(crate) mod typst;
pub(crate) mod words;

use minijinja::{Value, value::ValueKind};

use crate::error::TemplateError;
use crate::value::DecimalObject;

/// The locale used when a filter call does not pass one.
pub(crate) const DEFAULT_LOCALE: &str = "en";

/// Formatting rules for a locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocaleInfo {
    /// Locale identifier.
    pub(crate) id: &'static str,
    /// Digit group sizes, seen from the right; the last size repeats.
    pub(crate) pattern: &'static [usize],
    /// Digit group separator.
    pub(crate) group: char,
    /// Decimal separator.
    pub(crate) decimal: char,
}

/// Currency display data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CurrencyInfo {
    /// ISO 4217 code, matched exactly.
    pub(crate) code: &'static str,
    /// Symbol printed next to the amount.
    pub(crate) symbol: &'static str,
    /// `true`: symbol before the amount; `false`: after.
    pub(crate) prefix: bool,
    /// `true`: a space separates symbol and amount.
    pub(crate) space: bool,
    /// Fraction digit count (minor units).
    pub(crate) minor_units: u32,
    /// Name of exactly one whole unit.
    pub(crate) major_singular: &'static str,
    /// Name of zero or several whole units.
    pub(crate) major_plural: &'static str,
    /// Name of exactly one minor unit.
    pub(crate) minor_singular: &'static str,
    /// Name of zero or several minor units.
    pub(crate) minor_plural: &'static str,
}

const LOCALES: [LocaleInfo; 3] = [
    LocaleInfo {
        id: "en",
        pattern: &[3],
        group: ',',
        decimal: '.',
    },
    LocaleInfo {
        id: "en-IN",
        pattern: &[3, 2],
        group: ',',
        decimal: '.',
    },
    LocaleInfo {
        id: "ur-PK",
        pattern: &[3],
        group: ',',
        decimal: '.',
    },
];

/// Looks up a locale by identifier; unknown identifiers are errors.
pub(crate) fn locale(id: &str) -> Result<&'static LocaleInfo, TemplateError> {
    LOCALES
        .iter()
        .find(|entry| entry.id == id)
        .ok_or(TemplateError::UnknownLocale { id: id.to_owned() })
}

const CURRENCIES: [CurrencyInfo; 8] = [
    CurrencyInfo {
        code: "PKR",
        symbol: "Rs",
        prefix: true,
        space: true,
        minor_units: 2,
        major_singular: "Rupee",
        major_plural: "Rupees",
        minor_singular: "Paisa",
        minor_plural: "Paisa",
    },
    CurrencyInfo {
        code: "USD",
        symbol: "$",
        prefix: true,
        space: false,
        minor_units: 2,
        major_singular: "Dollar",
        major_plural: "Dollars",
        minor_singular: "Cent",
        minor_plural: "Cents",
    },
    CurrencyInfo {
        code: "EUR",
        symbol: "€",
        prefix: true,
        space: false,
        minor_units: 2,
        major_singular: "Euro",
        major_plural: "Euro",
        minor_singular: "Cent",
        minor_plural: "Cents",
    },
    CurrencyInfo {
        code: "GBP",
        symbol: "£",
        prefix: true,
        space: false,
        minor_units: 2,
        major_singular: "Pound",
        major_plural: "Pounds",
        minor_singular: "Penny",
        minor_plural: "Pence",
    },
    CurrencyInfo {
        code: "AED",
        symbol: "AED",
        prefix: true,
        space: true,
        minor_units: 2,
        major_singular: "Dirham",
        major_plural: "Dirhams",
        minor_singular: "Fils",
        minor_plural: "Fils",
    },
    CurrencyInfo {
        code: "SAR",
        symbol: "SAR",
        prefix: true,
        space: true,
        minor_units: 2,
        major_singular: "Riyal",
        major_plural: "Riyals",
        minor_singular: "Halala",
        minor_plural: "Halalas",
    },
    CurrencyInfo {
        code: "CNY",
        symbol: "¥",
        prefix: true,
        space: false,
        minor_units: 2,
        major_singular: "Yuan",
        major_plural: "Yuan",
        minor_singular: "Fen",
        minor_plural: "Fen",
    },
    CurrencyInfo {
        code: "INR",
        symbol: "₹",
        prefix: true,
        space: false,
        minor_units: 2,
        major_singular: "Rupee",
        major_plural: "Rupees",
        minor_singular: "Paisa",
        minor_plural: "Paise",
    },
];

/// Looks up a currency by its ISO 4217 code; unknown codes are errors.
pub(crate) fn currency(code: &str) -> Result<&'static CurrencyInfo, TemplateError> {
    CURRENCIES
        .iter()
        .find(|entry| entry.code == code)
        .ok_or(TemplateError::UnknownCurrency {
            code: code.to_owned(),
        })
}

/// The digit split of an exact amount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AmountDigits {
    /// `true` when the amount is negative.
    pub(crate) negative: bool,
    /// Integer digits, ASCII, at least one.
    pub(crate) int_part: String,
    /// Fraction digits, ASCII, possibly empty.
    pub(crate) frac: String,
    /// The accepted text, for error messages.
    pub(crate) raw: String,
}

/// Splits an exact amount into sign, integer and fraction digits.
///
/// Accepts the crate's decimal objects and exact integers; anything else is
/// a [`TemplateError::InvalidInput`].
pub(crate) fn split_amount(value: &Value, what: &str) -> Result<AmountDigits, TemplateError> {
    // Only decimal objects (any value kind) and exact number values
    // qualify; booleans, strings, undefined and none values are rejected
    // up front (they would otherwise silently coerce).
    if value.downcast_object_ref::<DecimalObject>().is_none()
        && !matches!(value.kind(), ValueKind::Number)
    {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "{what} expects an exact decimal amount or an integer; the value passed is not an exact amount"
            ),
        });
    }
    let text = if let Some(object) = value.downcast_object_ref::<DecimalObject>() {
        object.amount().to_string()
    } else if let Some(count) = value.as_i64() {
        count.to_string()
    } else if value.is_number() {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "{what} expects an exact decimal amount or an in-range integer; \
                 amounts beyond i64 must arrive as Decimals"
            ),
        });
    } else {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "{what} expects an exact decimal amount or an integer; \
                 the template passed an unsupported value"
            ),
        });
    };
    parse_digits(&text)
}

fn parse_digits(text: &str) -> Result<AmountDigits, TemplateError> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int_part, frac) = match rest.split_once('.') {
        Some((int_part, frac)) => (int_part, frac),
        None => (rest, ""),
    };
    if int_part.is_empty() || !int_part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TemplateError::InvalidInput {
            message: format!("amount '{text}' has no usable integer part"),
        });
    }
    if !frac.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TemplateError::InvalidInput {
            message: format!("amount '{text}' has a fraction with non-digits"),
        });
    }
    Ok(AmountDigits {
        negative,
        int_part: int_part.to_owned(),
        frac: frac.to_owned(),
        raw: text.to_owned(),
    })
}

/// Groups `digits` (ASCII) from the right following `pattern`; the pattern's
/// last size repeats to the left. `en` (3): `1234567` → `1,234,567`;
/// `en-IN` (3, 2): `1234567` → `12,34,567`.
pub(crate) fn group_digits(digits: &str, pattern: &[usize], separator: char) -> String {
    if digits.is_empty() || pattern.is_empty() {
        return digits.to_owned();
    }
    let mut groups: Vec<&str> = Vec::new();
    let mut end = digits.len();
    let mut index = 0;
    while end > 0 {
        let size = match pattern.get(index) {
            Some(size) => *size,
            None => *pattern.last().unwrap_or(&3),
        };
        let start = end.saturating_sub(size);
        groups.push(&digits[start..end]);
        end = start;
        index = index.saturating_add(1);
    }
    let mut out = String::with_capacity(digits.len() + 2);
    let mut first = true;
    for group in groups.iter().rev() {
        if !first {
            out.push(separator);
        }
        first = false;
        out.push_str(group);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn decimal(text: &str) -> Value {
        Value::from_object(DecimalObject::new(
            text.parse::<Decimal>()
                .unwrap_or_else(|error| panic!("bad decimal {text}: {error}")),
        ))
    }

    #[test]
    fn locale_lookup() {
        let info = locale("en-IN").unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(info.pattern, [3usize, 2].as_slice());
        assert_eq!(info.decimal, '.');
        match locale("xx") {
            Err(TemplateError::UnknownLocale { id }) => assert_eq!(id, "xx"),
            other => panic!("expected UnknownLocale, got {other:?}"),
        }
    }

    #[test]
    fn currency_lookup() {
        assert_eq!(
            currency("PKR")
                .unwrap_or_else(|e| panic!("{e}"))
                .minor_units,
            2
        );
        match currency("XYZ") {
            Err(TemplateError::UnknownCurrency { code }) => assert_eq!(code, "XYZ"),
            other => panic!("expected UnknownCurrency, got {other:?}"),
        }
    }

    #[test]
    fn splits_decimal_text() {
        let digits = split_amount(&decimal("1234.50"), "money").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(digits.int_part, "1234");
        assert_eq!(digits.frac, "50");
        assert!(!digits.negative);
        assert_eq!(digits.raw, "1234.50");
    }

    #[test]
    fn splits_negative_integer() {
        let digits = split_amount(&Value::from(-42_i64), "money").unwrap_or_else(|e| panic!("{e}"));
        assert!(digits.negative);
        assert_eq!(digits.int_part, "42");
        assert!(digits.frac.is_empty());
    }

    #[test]
    fn rejects_bools_strings_and_undef() {
        for value in [Value::from(true), Value::from("12.30"), Value::from(())] {
            match split_amount(&value, "money") {
                Err(TemplateError::InvalidInput { .. }) => {}
                other => panic!("expected InvalidInput, got {other:?}"),
            }
        }
    }

    #[test]
    fn en_grouping() {
        assert_eq!(group_digits("1234567", &[3], ','), "1,234,567");
        assert_eq!(group_digits("100", &[3], ','), "100");
        assert_eq!(group_digits("7", &[3], ','), "7");
    }

    #[test]
    fn south_asian_grouping() {
        assert_eq!(group_digits("1234567", &[3, 2], ','), "12,34,567");
        assert_eq!(group_digits("1000000", &[3, 2], ','), "10,00,000");
        assert_eq!(group_digits("500", &[3, 2], ','), "500");
    }
}
