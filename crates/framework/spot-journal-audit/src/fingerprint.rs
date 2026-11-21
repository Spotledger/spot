//! The financial fingerprint compare (ADR-061 s8, WP-MIG-06): the named numeric sets — Trial
//! Balance, Stock Balance, AR and AP ageing — of the live database against the scratch rebuild.
//!
//! A value is **never a float** (ADR-12): it is either an integer in the currency's minor unit or
//! an exact decimal given as a string, which this module parses into a sign, digits and a scale.
//! The two forms are never converted into one another — a minor unit has no currency and so no
//! implied exponent — so both sides must use the same form for a value, and a form mismatch is
//! reported as a difference rather than reconciled silently.
//!
//! Comparison is exact: `1.5`, `1.50` and `01.500` are the same amount, and `0.0100000001` is not
//! `0.01`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::error::AuditError;

/// An exact decimal: a sign, the digits without leading or trailing zeros, and a scale.
///
/// A decimal is built by [`Decimal::parse`], so a value that exists has a canonical form and two
/// values compare equal exactly when they are the same number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decimal {
    /// Whether the amount is negative; a zero is never negative.
    negative: bool,
    /// The significant digits, no leading zero, never empty (`0` for a zero).
    digits: String,
    /// How many of the digits are fractional.
    scale: u32,
}

impl Decimal {
    /// Parses an exact decimal string: an optional sign, one or more whole digits, and an optional
    /// point followed by one or more fractional digits. No exponent, no separators, no spaces.
    ///
    /// # Errors
    ///
    /// [`AuditError::AmountInvalid`] when the text is not that form.
    pub fn parse(text: &str) -> Result<Self, AuditError> {
        let invalid = |reason: &'static str| AuditError::AmountInvalid {
            text: text.to_owned(),
            reason,
        };
        let (negative, unsigned) = match text.as_bytes().first() {
            Some(b'-') => (true, &text[1..]),
            Some(b'+') => (false, &text[1..]),
            _ => (false, text),
        };
        if unsigned.is_empty() {
            return Err(invalid("it has no digits"));
        }
        let mut parts = unsigned.split('.');
        let whole = parts.next().unwrap_or("");
        let fraction = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(invalid("it has more than one decimal point"));
        }
        if whole.is_empty() {
            return Err(invalid("it has no whole digits"));
        }
        if !whole.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("it has a character that is not a digit"));
        }
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("it has a character that is not a digit"));
        }
        let has_point = unsigned.contains('.');
        if has_point && fraction.is_empty() {
            return Err(invalid("it has no fractional digits after the point"));
        }
        let scale = u32::try_from(fraction.len()).map_err(|_| invalid("it has too many digits"))?;

        let mut digits = String::with_capacity(whole.len() + fraction.len());
        digits.push_str(whole);
        digits.push_str(fraction);
        let trimmed = digits.trim_start_matches('0');
        let mut kept = if trimmed.is_empty() {
            String::new()
        } else {
            trimmed.to_owned()
        };
        let mut scale = scale;
        while scale > 0 && kept.ends_with('0') {
            kept.pop();
            scale -= 1;
        }
        if kept.is_empty() {
            return Ok(Self {
                negative: false,
                digits: "0".to_owned(),
                scale: 0,
            });
        }
        Ok(Self {
            negative,
            digits: kept,
            scale,
        })
    }

    /// Whether the amount is zero (`0`, `0.00` and `-0.0` are all zero).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.digits == "0"
    }
}

impl fmt::Display for Decimal {
    /// The canonical form: no leading or trailing zeros, no sign on a zero.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        if self.scale == 0 {
            return f.write_str(&self.digits);
        }
        let scale = self.scale as usize;
        if self.digits.len() > scale {
            let (whole, fraction) = self.digits.split_at(self.digits.len() - scale);
            write!(f, "{whole}.{fraction}")
        } else {
            let zeros = "0".repeat(scale - self.digits.len());
            write!(f, "0.{zeros}{}", self.digits)
        }
    }
}

/// One financial value: integer minor units, or an exact decimal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Amount {
    /// An integer amount in the currency's minor unit (cents, pence, fils).
    Minor(i64),
    /// An exact decimal amount, already parsed so it has a canonical form.
    Decimal(Decimal),
}

impl Amount {
    /// An amount in minor units.
    #[must_use]
    pub fn minor(units: i64) -> Self {
        Self::Minor(units)
    }

    /// An amount from an exact decimal string.
    ///
    /// # Errors
    ///
    /// As [`Decimal::parse`].
    pub fn decimal(text: &str) -> Result<Self, AuditError> {
        Ok(Self::Decimal(Decimal::parse(text)?))
    }

    /// Whether the amount is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        match self {
            Self::Minor(units) => *units == 0,
            Self::Decimal(decimal) => decimal.is_zero(),
        }
    }

    /// The canonical text of the amount, for a log line or a diff: minor units as a plain integer,
    /// a decimal in its canonical form.
    #[must_use]
    pub fn to_canonical_string(&self) -> String {
        match self {
            Self::Minor(units) => units.to_string(),
            Self::Decimal(decimal) => decimal.to_string(),
        }
    }
}

/// One value of a fingerprint: the key the declared query grouped by, and the amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintValue {
    /// The key the declared query grouped this amount by (an account, an item and warehouse, an
    /// ageing bucket).
    pub key: String,
    /// The amount summed for that key.
    pub amount: Amount,
}

/// One named numeric set: what a declared fingerprint query summed, by key.
///
/// Values are sorted by key and a key appears once, so two fingerprints of the same contents are
/// equal whatever order their rows arrived in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// The set's name, as the tenant's fingerprint declares it (`trial_balance`,
    /// `stock_balance`, `ar_ageing`, `ap_ageing`).
    pub name: String,
    /// The amounts by key, sorted by key.
    pub values: Vec<FingerprintValue>,
}

impl Fingerprint {
    /// A fingerprint from its name and its amounts; the keys are sorted and must be unique and
    /// non-empty.
    ///
    /// # Errors
    ///
    /// [`AuditError::FingerprintInvalid`] when the name is empty, a key is empty, or a key repeats.
    pub fn new(
        name: impl Into<String>,
        values: impl IntoIterator<Item = (String, Amount)>,
    ) -> Result<Self, AuditError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(AuditError::FingerprintInvalid {
                fingerprint: "(unnamed)".to_owned(),
                reason: "the fingerprint has no name",
            });
        }
        let mut values: Vec<FingerprintValue> = values
            .into_iter()
            .map(|(key, amount)| FingerprintValue { key, amount })
            .collect();
        if values.iter().any(|value| value.key.trim().is_empty()) {
            return Err(AuditError::FingerprintInvalid {
                fingerprint: name,
                reason: "a key is empty",
            });
        }
        values.sort_by(|a, b| a.key.cmp(&b.key));
        if values.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(AuditError::FingerprintInvalid {
                fingerprint: name,
                reason: "a key repeats",
            });
        }
        Ok(Self { name, values })
    }

    /// The amount under a key, if the fingerprint has it.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&Amount> {
        self.values
            .binary_search_by(|value| value.key.as_str().cmp(key))
            .ok()
            .map(|index| &self.values[index].amount)
    }

    /// Whether the fingerprint holds no value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// How a value differs between the two sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueDifferenceKind {
    /// The live database has the key and the rebuild does not.
    MissingInRebuild,
    /// The rebuild has the key and the live database does not.
    ExtraInRebuild,
    /// Both have the key with a different amount.
    ValueDiffers,
}

/// One differing value of a fingerprint, with both sides' amounts where they exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueDifference {
    /// The key the amounts are grouped under.
    pub key: String,
    /// How the value differs.
    pub kind: ValueDifferenceKind,
    /// The live database's amount, when it has the key.
    pub live: Option<Amount>,
    /// The rebuild's amount, when it has the key.
    pub rebuilt: Option<Amount>,
}

/// Whether a whole fingerprint is missing, extra or different.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintDifferenceKind {
    /// The live database has this fingerprint and the rebuild does not.
    MissingInRebuild,
    /// The rebuild has this fingerprint and the live database does not.
    ExtraInRebuild,
    /// Both have it with different values.
    ValuesDiffer,
}

/// One fingerprint that does not agree, with every value that differs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintDifference {
    /// The fingerprint's name.
    pub name: String,
    /// How the fingerprint differs.
    pub kind: FingerprintDifferenceKind,
    /// The values that differ, in key order; empty when the whole fingerprint is missing or extra.
    pub values: Vec<ValueDifference>,
}

/// Every fingerprint that does not agree between the two sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintReport {
    /// The differing fingerprints, in name order.
    pub differing: Vec<FingerprintDifference>,
}

impl FingerprintReport {
    /// Whether every fingerprint agrees.
    #[must_use]
    pub fn is_match(&self) -> bool {
        self.differing.is_empty()
    }

    /// The names of the fingerprints that differ, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.differing
            .iter()
            .map(|difference| difference.name.as_str())
            .collect()
    }
}

/// Compares the financial fingerprints of the live database and the scratch rebuild, reporting
/// every name that differs and, inside it, every key that differs.
///
/// Both sides are slices, not streams: a fingerprint is small by construction (one amount per
/// declared key), unlike the records of [`compare`](crate::compare).
///
/// # Errors
///
/// [`AuditError::FingerprintInvalid`] when either side names the same fingerprint twice, because
/// the two could not be told apart.
pub fn compare_fingerprints(
    live: &[Fingerprint],
    rebuilt: &[Fingerprint],
) -> Result<FingerprintReport, AuditError> {
    let live = by_name(live)?;
    let rebuilt = by_name(rebuilt)?;
    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(live.keys().copied());
    names.extend(rebuilt.keys().copied());

    let mut differing = Vec::new();
    for name in names {
        match (live.get(name), rebuilt.get(name)) {
            (Some(left), Some(right)) => {
                let values = compare_values(&left.values, &right.values);
                if !values.is_empty() {
                    differing.push(FingerprintDifference {
                        name: name.to_owned(),
                        kind: FingerprintDifferenceKind::ValuesDiffer,
                        values,
                    });
                }
            }
            (Some(_), None) => differing.push(FingerprintDifference {
                name: name.to_owned(),
                kind: FingerprintDifferenceKind::MissingInRebuild,
                values: Vec::new(),
            }),
            (None, Some(_)) => differing.push(FingerprintDifference {
                name: name.to_owned(),
                kind: FingerprintDifferenceKind::ExtraInRebuild,
                values: Vec::new(),
            }),
            (None, None) => {}
        }
    }
    Ok(FingerprintReport { differing })
}

/// The fingerprints of one side by name, refusing a name that appears twice.
fn by_name(fingerprints: &[Fingerprint]) -> Result<BTreeMap<&str, &Fingerprint>, AuditError> {
    let mut by_name: BTreeMap<&str, &Fingerprint> = BTreeMap::new();
    for fingerprint in fingerprints {
        if by_name
            .insert(fingerprint.name.as_str(), fingerprint)
            .is_some()
        {
            return Err(AuditError::FingerprintInvalid {
                fingerprint: fingerprint.name.clone(),
                reason: "two fingerprints share the name",
            });
        }
    }
    Ok(by_name)
}

/// Compares two value lists by key, reporting every key that differs.
fn compare_values(live: &[FingerprintValue], rebuilt: &[FingerprintValue]) -> Vec<ValueDifference> {
    let mut left: BTreeMap<&str, &Amount> = live
        .iter()
        .map(|value| (value.key.as_str(), &value.amount))
        .collect();
    let mut right: BTreeMap<&str, &Amount> = rebuilt
        .iter()
        .map(|value| (value.key.as_str(), &value.amount))
        .collect();
    let keys: BTreeSet<&str> = left.keys().chain(right.keys()).copied().collect();
    let mut differences = Vec::new();
    for key in keys {
        match (left.remove(key), right.remove(key)) {
            (Some(l), Some(r)) if l != r => differences.push(ValueDifference {
                key: key.to_owned(),
                kind: ValueDifferenceKind::ValueDiffers,
                live: Some(l.clone()),
                rebuilt: Some(r.clone()),
            }),
            (Some(l), None) => differences.push(ValueDifference {
                key: key.to_owned(),
                kind: ValueDifferenceKind::MissingInRebuild,
                live: Some(l.clone()),
                rebuilt: None,
            }),
            (None, Some(r)) => differences.push(ValueDifference {
                key: key.to_owned(),
                kind: ValueDifferenceKind::ExtraInRebuild,
                live: None,
                rebuilt: Some(r.clone()),
            }),
            _ => {}
        }
    }
    differences
}
