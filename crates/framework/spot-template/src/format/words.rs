//! The `amount_in_words` renderer (English only in WP-PLT-12).

use minijinja::Value;

use crate::error::TemplateError;
use crate::format::{CurrencyInfo, split_amount};

/// The supported numbering systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberSystem {
    /// Short scale: thousand, million, billion, trillion.
    International,
    /// South Asian numbering: thousand, lakh, crore.
    SouthAsian,
}

const ONES: [&str; 20] = [
    "Zero",
    "One",
    "Two",
    "Three",
    "Four",
    "Five",
    "Six",
    "Seven",
    "Eight",
    "Nine",
    "Ten",
    "Eleven",
    "Twelve",
    "Thirteen",
    "Fourteen",
    "Fifteen",
    "Sixteen",
    "Seventeen",
    "Eighteen",
    "Nineteen",
];

const TENS: [&str; 10] = [
    "", "", "Twenty", "Thirty", "Forty", "Fifty", "Sixty", "Seventy", "Eighty", "Ninety",
];

/// Integer parts above 999,999,999,999,999 are not supported in words.
const MAX_INTEGER_DIGITS: usize = 15;

/// Renders an amount in words: `"One Hundred Twenty-Three Thousand Four
/// Hundred Fifty-Six Rupees and Seventy-Eight Paisa Only"`.
pub(crate) fn amount_in_words(
    value: &Value,
    currency: &CurrencyInfo,
    lang: &str,
    system: NumberSystem,
) -> Result<String, TemplateError> {
    if lang != "en" {
        return Err(TemplateError::UnknownLocale {
            id: lang.to_owned(),
        });
    }
    let digits = split_amount(value, "amount_in_words")?;
    let minor_units = currency.minor_units as usize;
    if digits.frac.len() > minor_units {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "amount '{}' has {} fractional digits but {} has {} minor units; \
                 money is rounded upstream (ADR-12)",
                digits.raw,
                digits.frac.len(),
                currency.code,
                currency.minor_units
            ),
        });
    }
    if digits.int_part.len() > MAX_INTEGER_DIGITS {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "amount '{}' is too large to spell in words (at most {} \
                 integer digits are supported)",
                digits.raw, MAX_INTEGER_DIGITS
            ),
        });
    }
    let int_value: u64 = match digits.int_part.parse::<u64>() {
        Ok(value) => value,
        Err(error) => {
            return Err(TemplateError::InvalidInput {
                message: format!("amount '{}' cannot be read as a count: {error}", digits.raw),
            });
        }
    };
    let minor: u64 = if digits.frac.is_empty() {
        0
    } else {
        match digits.frac.parse::<u64>() {
            Ok(value) => value,
            Err(error) => {
                return Err(TemplateError::InvalidInput {
                    message: format!(
                        "amount '{}': fraction could not be read: {error}",
                        digits.raw
                    ),
                });
            }
        }
    };
    if minor >= 1_000 {
        return Err(TemplateError::InvalidInput {
            message: format!(
                "amount '{}' has more than three minor digits; not supported in words",
                digits.raw
            ),
        });
    }
    let major_unit = if int_value == 1 {
        currency.major_singular
    } else {
        currency.major_plural
    };
    let minor_unit = if minor == 1 {
        currency.minor_singular
    } else {
        currency.minor_plural
    };
    let int_words = if int_value == 0 {
        "Zero".to_owned()
    } else {
        words_for(int_value, system)
    };
    let mut parts: Vec<String> = vec![format!("{int_words} {major_unit}")];
    if minor > 0 {
        parts.push(format!("{} {minor_unit}", under_thousand(minor)));
    }
    let mut out = String::new();
    if digits.negative {
        out.push_str("Minus ");
    }
    out.push_str(&parts.join(" and "));
    out.push_str(" Only");
    Ok(out)
}

fn words_for(value: u64, system: NumberSystem) -> String {
    match system {
        NumberSystem::International => international_words(value),
        NumberSystem::SouthAsian => south_asian_words(value),
    }
}

/// Words for 0..=999; empty for 0.
fn under_thousand(value: u64) -> String {
    if value == 0 {
        return String::new();
    }
    let mut out = String::new();
    let hundreds = value / 100;
    let rest = value % 100;
    if hundreds > 0 {
        out.push_str(ONES[hundreds as usize]);
        out.push_str(" Hundred");
    }
    if rest > 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&under_hundred(rest));
    }
    out
}

fn under_hundred(value: u64) -> String {
    if value < 20 {
        return ONES[value as usize].to_owned();
    }
    let ten = TENS[(value / 10) as usize];
    let unit = value % 10;
    if unit == 0 {
        ten.to_owned()
    } else {
        format!("{ten}-{}", ONES[unit as usize])
    }
}

fn international_words(value: u64) -> String {
    if value == 0 {
        return "Zero".to_owned();
    }
    const SCALES: [(u64, &str); 4] = [
        (1_000_000_000_000, "Trillion"),
        (1_000_000_000, "Billion"),
        (1_000_000, "Million"),
        (1_000, "Thousand"),
    ];
    let mut parts: Vec<String> = Vec::new();
    let mut rest = value;
    for (scale, name) in SCALES {
        if rest >= scale {
            let quotient = rest / scale;
            rest %= scale;
            parts.push(format!("{} {name}", under_thousand(quotient)));
        }
    }
    let remainder = under_thousand(rest);
    if !remainder.is_empty() {
        parts.push(remainder);
    }
    if parts.is_empty() {
        "Zero".to_owned()
    } else {
        parts.join(" ")
    }
}

/// South Asian words with recursive crore groups, so amounts up to the
/// supported 999,999,999,999,999 stay correct.
fn south_asian_words(value: u64) -> String {
    if value >= 10_000_000 {
        let crore = value / 10_000_000;
        let rest = value % 10_000_000;
        let mut out = format!("{} Crore", south_asian_words(crore));
        if rest > 0 {
            out.push(' ');
            out.push_str(&south_asian_words(rest));
        }
        return out;
    }
    if value >= 100_000 {
        let lakh = value / 100_000;
        let rest = value % 100_000;
        let mut out = format!("{} Lakh", under_thousand(lakh));
        if rest > 0 {
            out.push(' ');
            out.push_str(&south_asian_words(rest));
        }
        return out;
    }
    if value >= 1_000 {
        let thousand = value / 1_000;
        let rest = value % 1_000;
        let mut out = format!("{} Thousand", under_thousand(thousand));
        if rest > 0 {
            out.push(' ');
            out.push_str(&south_asian_words(rest));
        }
        return out;
    }
    under_thousand(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::currency;
    use crate::value::DecimalObject;
    use rust_decimal::Decimal;

    fn amount(text: &str) -> Value {
        Value::from_object(DecimalObject::new(
            text.parse::<Decimal>()
                .unwrap_or_else(|error| panic!("bad decimal: {error}")),
        ))
    }

    fn words(value: &Value, code: &str, system: NumberSystem) -> String {
        amount_in_words(
            value,
            currency(code).unwrap_or_else(|e| panic!("{e}")),
            "en",
            system,
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn table_covers_the_brief_list_in_both_systems() {
        // (amount, international, south-asian)
        let cases: [(&str, &str, &str); 28] = [
            ("0", "Zero Rupees Only", "Zero Rupees Only"),
            (
                "0.05",
                "Zero Rupees and Five Paisa Only",
                "Zero Rupees and Five Paisa Only",
            ),
            ("1", "One Rupee Only", "One Rupee Only"),
            ("2", "Two Rupees Only", "Two Rupees Only"),
            ("7", "Seven Rupees Only", "Seven Rupees Only"),
            ("10", "Ten Rupees Only", "Ten Rupees Only"),
            ("11", "Eleven Rupees Only", "Eleven Rupees Only"),
            ("15", "Fifteen Rupees Only", "Fifteen Rupees Only"),
            ("19", "Nineteen Rupees Only", "Nineteen Rupees Only"),
            ("20", "Twenty Rupees Only", "Twenty Rupees Only"),
            ("21", "Twenty-One Rupees Only", "Twenty-One Rupees Only"),
            ("40", "Forty Rupees Only", "Forty Rupees Only"),
            (
                "77",
                "Seventy-Seven Rupees Only",
                "Seventy-Seven Rupees Only",
            ),
            ("90", "Ninety Rupees Only", "Ninety Rupees Only"),
            ("100", "One Hundred Rupees Only", "One Hundred Rupees Only"),
            (
                "101",
                "One Hundred One Rupees Only",
                "One Hundred One Rupees Only",
            ),
            (
                "110",
                "One Hundred Ten Rupees Only",
                "One Hundred Ten Rupees Only",
            ),
            (
                "210",
                "Two Hundred Ten Rupees Only",
                "Two Hundred Ten Rupees Only",
            ),
            (
                "1000",
                "One Thousand Rupees Only",
                "One Thousand Rupees Only",
            ),
            ("1000000", "One Million Rupees Only", "Ten Lakh Rupees Only"),
            (
                "1001",
                "One Thousand One Rupees Only",
                "One Thousand One Rupees Only",
            ),
            (
                "1234",
                "One Thousand Two Hundred Thirty-Four Rupees Only",
                "One Thousand Two Hundred Thirty-Four Rupees Only",
            ),
            (
                "500050",
                "Five Hundred Thousand Fifty Rupees Only",
                "Five Lakh Fifty Rupees Only",
            ),
            (
                "99999",
                "Ninety-Nine Thousand Nine Hundred Ninety-Nine Rupees Only",
                "Ninety-Nine Thousand Nine Hundred Ninety-Nine Rupees Only",
            ),
            (
                "100000",
                "One Hundred Thousand Rupees Only",
                "One Lakh Rupees Only",
            ),
            (
                "100001",
                "One Hundred Thousand One Rupees Only",
                "One Lakh One Rupees Only",
            ),
            (
                "10000000",
                "Ten Million Rupees Only",
                "One Crore Rupees Only",
            ),
            (
                "999999999999999.99",
                "Nine Hundred Ninety-Nine Trillion Nine Hundred Ninety-Nine Billion Nine Hundred Ninety-Nine Million Nine Hundred Ninety-Nine Thousand Nine Hundred Ninety-Nine Rupees and Ninety-Nine Paisa Only",
                "Nine Crore Ninety-Nine Lakh Ninety-Nine Thousand Nine Hundred Ninety-Nine Crore Ninety-Nine Lakh Ninety-Nine Thousand Nine Hundred Ninety-Nine Rupees and Ninety-Nine Paisa Only",
            ),
        ];
        for (amount_text, international, south_asian_expected) in cases {
            assert_eq!(
                words(&amount(amount_text), "PKR", NumberSystem::International),
                international,
                "international {amount_text}"
            );
            assert_eq!(
                words(&amount(amount_text), "PKR", NumberSystem::SouthAsian),
                south_asian_expected,
                "south-asian {amount_text}"
            );
        }
    }

    #[test]
    fn brief_example() {
        assert_eq!(
            words(&amount("123456.78"), "PKR", NumberSystem::International),
            "One Hundred Twenty-Three Thousand Four Hundred Fifty-Six Rupees and Seventy-Eight Paisa Only"
        );
    }

    #[test]
    fn zero_one_eleven() {
        assert_eq!(
            words(&amount("0"), "PKR", NumberSystem::International),
            "Zero Rupees Only"
        );
        assert_eq!(
            words(&amount("1"), "PKR", NumberSystem::International),
            "One Rupee Only"
        );
        assert_eq!(
            words(&amount("11"), "PKR", NumberSystem::International),
            "Eleven Rupees Only"
        );
    }

    #[test]
    fn paisa_only_and_negative() {
        assert_eq!(
            words(&amount("0.05"), "PKR", NumberSystem::International),
            "Zero Rupees and Five Paisa Only"
        );
        assert_eq!(
            words(&amount("-50"), "PKR", NumberSystem::International),
            "Minus Fifty Rupees Only"
        );
    }

    #[test]
    fn lakh_and_crore() {
        assert_eq!(
            words(&amount("125000"), "PKR", NumberSystem::SouthAsian),
            "One Lakh Twenty-Five Thousand Rupees Only"
        );
        assert_eq!(
            words(&amount("12345678"), "PKR", NumberSystem::SouthAsian),
            "One Crore Twenty-Three Lakh Forty-Five Thousand Six Hundred Seventy-Eight Rupees Only"
        );
    }

    #[test]
    fn international_million() {
        assert_eq!(
            words(&amount("123456789"), "PKR", NumberSystem::International),
            "One Hundred Twenty-Three Million Four Hundred Fifty-Six Thousand Seven Hundred Eighty-Nine Rupees Only"
        );
    }

    #[test]
    fn unknown_language_is_an_error() {
        match amount_in_words(
            &amount("1"),
            currency("PKR").unwrap_or_else(|e| panic!("{e}")),
            "ur",
            NumberSystem::International,
        ) {
            Err(TemplateError::UnknownLocale { id }) => assert_eq!(id, "ur"),
            other => panic!("expected UnknownLocale, got {other:?}"),
        }
    }

    #[test]
    fn oversized_integer_is_an_error() {
        match amount_in_words(
            &amount("12345678901234567.00"),
            currency("PKR").unwrap_or_else(|e| panic!("{e}")),
            "en",
            NumberSystem::International,
        ) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("too large"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
}
