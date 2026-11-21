//! The financial fingerprint: a deterministic, integer-only structure a rehearsal measures
//! before and after it applies a plan, plus the digest and the diff.
//!
//! A [`Fingerprint`] is a list of [`FingerprintSection`]s. A section is the result of one declared
//! [`FingerprintQuery`](crate::FingerprintQuery): a set of dimension values (an account and a
//! company, an item, a warehouse and a valuation, an ageing bucket) and one integer measure per
//! declared measure name. Nothing here is money-typed: a measure is the integer minor unit the
//! declared query already summed (ADR-12 forbids floating point).
//!
//! Sections are sorted by key and rows by their dimensions, so the canonical JSON of a
//! [`Fingerprint`] — and therefore its [`digest`](Fingerprint::digest) — depends only on its
//! contents, never on the order rows arrived in.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One dimension of a fingerprint row: a declared name and its value, as text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dimension {
    /// The declared dimension name, for example `account` or `company`.
    pub name: String,
    /// The value, as text (an id, a code or a bucket label).
    pub value: String,
}

impl Dimension {
    /// A dimension.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Dimension {
        Dimension {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// One fingerprint row: the dimension values that identify it and one integer per measure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintRow {
    /// The dimension values, sorted by name.
    pub dimensions: Vec<Dimension>,
    /// The measures, keyed by name; a missing measure reads as zero.
    ///
    /// Serialised as decimal strings (`{"debit":"-12345678901234567890123"}`), always. serde cannot
    /// buffer an `i128`, so a JSON integer here could not be read back from inside an internally
    /// tagged enum such as a journal record. A JSON integer that fits `i64`/`u64` is still read.
    #[serde(with = "crate::wire::map")]
    pub measures: BTreeMap<String, i128>,
}

impl FingerprintRow {
    /// A row from its dimensions and measures.
    pub fn new(dimensions: Vec<Dimension>, measures: BTreeMap<String, i128>) -> FingerprintRow {
        let mut dimensions = dimensions;
        dimensions.sort();
        FingerprintRow {
            dimensions,
            measures,
        }
    }

    /// The measure `name`, or zero when the row does not carry it.
    pub fn measure(&self, name: &str) -> i128 {
        self.measures.get(name).copied().unwrap_or(0)
    }

    /// The key a row is matched on across two fingerprints: its dimensions.
    fn key(&self) -> Vec<(String, String)> {
        self.dimensions
            .iter()
            .map(|d| (d.name.clone(), d.value.clone()))
            .collect()
    }
}

/// One section of a fingerprint: the declared dimensions and measures and the rows they produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintSection {
    /// The declared section key, for example `trial_balance`.
    pub key: String,
    /// A human title.
    pub title: String,
    /// The declared dimension names, in declaration order.
    pub dimensions: Vec<String>,
    /// The declared measure names, in declaration order.
    pub measures: Vec<String>,
    /// The rows, sorted by their dimensions.
    pub rows: Vec<FingerprintRow>,
}

impl FingerprintSection {
    /// A section with rows sorted by their dimensions.
    pub fn new(
        key: impl Into<String>,
        title: impl Into<String>,
        dimensions: Vec<String>,
        measures: Vec<String>,
        mut rows: Vec<FingerprintRow>,
    ) -> FingerprintSection {
        rows.sort_by(|a, b| a.dimensions.cmp(&b.dimensions));
        FingerprintSection {
            key: key.into(),
            title: title.into(),
            dimensions,
            measures,
            rows,
        }
    }
}

/// A whole fingerprint: every section, in a canonical order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The sections, sorted by key.
    pub sections: Vec<FingerprintSection>,
}

impl Fingerprint {
    /// A fingerprint from its sections; they are sorted by key.
    pub fn new(mut sections: Vec<FingerprintSection>) -> Fingerprint {
        sections.sort_by(|a, b| a.key.cmp(&b.key));
        Fingerprint { sections }
    }

    /// Whether the fingerprint has no section.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The section with this key, if any.
    pub fn section(&self, key: &str) -> Option<&FingerprintSection> {
        self.sections.iter().find(|s| s.key == key)
    }

    /// The canonical JSON of the fingerprint: deterministic for the same contents.
    ///
    /// Measures are written as JSON integers here, not in the string wire form of the serialised
    /// type, so the digest depends only on the numbers and stays what it has always been.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&Canon(self)).unwrap_or_default()
    }

    /// The SHA-256 hex digest of the canonical form.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        hex(&hasher.finalize())
    }

    /// Every measure that differs from `after`, one [`FingerprintDiff`] per difference.
    ///
    /// A row or measure present on one side only is a difference from (or to) zero. The result is
    /// ordered by section, then by the row's dimensions, then by measure name.
    pub fn diff(&self, after: &Fingerprint) -> Vec<FingerprintDiff> {
        let mut out = Vec::new();
        let keys: BTreeSet<&str> = self
            .sections
            .iter()
            .chain(after.sections.iter())
            .map(|s| s.key.as_str())
            .collect();
        for key in keys {
            let before = self.section(key);
            let after = after.section(key);
            let mut row_keys: BTreeSet<Vec<(String, String)>> = BTreeSet::new();
            if let Some(s) = before {
                row_keys.extend(s.rows.iter().map(FingerprintRow::key));
            }
            if let Some(s) = after {
                row_keys.extend(s.rows.iter().map(FingerprintRow::key));
            }
            for row_key in row_keys {
                let measures: BTreeSet<String> = before
                    .into_iter()
                    .flat_map(|s| s.measures.iter().cloned())
                    .chain(after.into_iter().flat_map(|s| s.measures.iter().cloned()))
                    .collect();
                for measure in measures {
                    let b = row_measure(before, &row_key, &measure);
                    let a = row_measure(after, &row_key, &measure);
                    if a != b {
                        out.push(FingerprintDiff {
                            section: key.to_owned(),
                            dimensions: row_key
                                .iter()
                                .map(|(name, value)| Dimension::new(name.clone(), value.clone()))
                                .collect(),
                            measure,
                            before: b,
                            after: a,
                        });
                    }
                }
            }
        }
        out
    }
}

/// The measure of the row keyed by `row_key` in `section`, or zero when absent.
fn row_measure(
    section: Option<&FingerprintSection>,
    row_key: &[(String, String)],
    measure: &str,
) -> i128 {
    let Some(section) = section else {
        return 0;
    };
    for row in &section.rows {
        if row.key() == row_key {
            return row.measure(measure);
        }
    }
    0
}

/// One measure that changed between two fingerprints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintDiff {
    /// The section key.
    pub section: String,
    /// The dimensions of the affected row.
    pub dimensions: Vec<Dimension>,
    /// The measure name.
    pub measure: String,
    /// The value before the plan was applied (a decimal string on the wire).
    #[serde(with = "crate::wire")]
    pub before: i128,
    /// The value after the plan was applied (a decimal string on the wire).
    #[serde(with = "crate::wire")]
    pub after: i128,
}

impl FingerprintDiff {
    /// `after - before`.
    pub fn delta(&self) -> i128 {
        self.after - self.before
    }
}

/// The digest form of a [`Fingerprint`]: the same shape as its serialised form, with integer
/// measures. Private so the digest cannot drift with the wire form.
struct Canon<'a>(&'a Fingerprint);

impl Serialize for Canon<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("Fingerprint", 1)?;
        let sections: Vec<CanonSection<'_>> = self.0.sections.iter().map(CanonSection).collect();
        st.serialize_field("sections", &sections)?;
        st.end()
    }
}

struct CanonSection<'a>(&'a FingerprintSection);

impl Serialize for CanonSection<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let s = self.0;
        let mut st = serializer.serialize_struct("FingerprintSection", 5)?;
        st.serialize_field("key", &s.key)?;
        st.serialize_field("title", &s.title)?;
        st.serialize_field("dimensions", &s.dimensions)?;
        st.serialize_field("measures", &s.measures)?;
        let rows: Vec<CanonRow<'_>> = s.rows.iter().map(CanonRow).collect();
        st.serialize_field("rows", &rows)?;
        st.end()
    }
}

struct CanonRow<'a>(&'a FingerprintRow);

impl Serialize for CanonRow<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("FingerprintRow", 2)?;
        st.serialize_field("dimensions", &self.0.dimensions)?;
        st.serialize_field("measures", &self.0.measures)?;
        st.end()
    }
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn row(dims: &[(&str, &str)], measures: &[(&str, i128)]) -> FingerprintRow {
        FingerprintRow::new(
            dims.iter().map(|(n, v)| Dimension::new(*n, *v)).collect(),
            measures
                .iter()
                .map(|(n, v)| ((*n).to_owned(), *v))
                .collect(),
        )
    }

    fn section(rows: Vec<FingerprintRow>) -> FingerprintSection {
        FingerprintSection::new(
            "trial_balance",
            "Trial balance",
            vec!["account".into(), "company".into()],
            vec!["debit".into(), "credit".into()],
            rows,
        )
    }

    #[test]
    fn the_digest_is_pinned_to_the_numbers_not_the_wire_form() {
        let f = Fingerprint::new(vec![section(vec![
            row(
                &[("account", "cash"), ("company", "acme")],
                &[("debit", 10), ("credit", -4)],
            ),
            row(
                &[("account", "bank"), ("company", "acme")],
                &[("debit", i128::MAX), ("credit", i128::MIN)],
            ),
        ])]);
        // Computed before the string wire form existed; it must never change.
        assert_eq!(
            f.digest(),
            "574dbc68488cf26f2cb5b8c8dc1457fa939a11b79b9e4c1dc0577423c49a995e"
        );
        // The serialised form is the string form, and differs from the digest form.
        let wire = serde_json::to_string(&f).unwrap();
        assert!(wire.contains(r#""credit":"-4""#));
        assert!(f.canonical_json().contains(r#""credit":-4"#));
    }

    #[test]
    fn digest_is_order_independent() {
        let a = Fingerprint::new(vec![section(vec![
            row(
                &[("account", "cash"), ("company", "acme")],
                &[("debit", 10)],
            ),
            row(
                &[("account", "bank"), ("company", "acme")],
                &[("debit", 20)],
            ),
        ])]);
        let b = Fingerprint::new(vec![section(vec![
            row(
                &[("account", "bank"), ("company", "acme")],
                &[("debit", 20)],
            ),
            row(
                &[("account", "cash"), ("company", "acme")],
                &[("debit", 10)],
            ),
        ])]);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn digest_changes_with_a_measure() {
        let a = Fingerprint::new(vec![section(vec![row(
            &[("account", "cash"), ("company", "acme")],
            &[("debit", 10)],
        )])]);
        let b = Fingerprint::new(vec![section(vec![row(
            &[("account", "cash"), ("company", "acme")],
            &[("debit", 11)],
        )])]);
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn an_empty_diff_for_equal_fingerprints() {
        let a = Fingerprint::new(vec![section(vec![row(
            &[("account", "cash"), ("company", "acme")],
            &[("debit", 10)],
        )])]);
        assert!(a.diff(&a).is_empty());
    }

    #[test]
    fn diff_reports_the_delta_and_missing_rows() {
        let before = Fingerprint::new(vec![section(vec![row(
            &[("account", "cash"), ("company", "acme")],
            &[("debit", 10), ("credit", 4)],
        )])]);
        let after = Fingerprint::new(vec![section(vec![
            row(
                &[("account", "cash"), ("company", "acme")],
                &[("debit", 10), ("credit", 5)],
            ),
            row(&[("account", "bank"), ("company", "acme")], &[("debit", 7)]),
        ])]);
        let diffs = before.diff(&after);
        assert_eq!(diffs.len(), 2);
        let credit = diffs
            .iter()
            .find(|d| d.measure == "credit")
            .expect("credit diff");
        assert_eq!(credit.delta(), 1);
        let bank = diffs
            .iter()
            .find(|d| d.dimensions.iter().any(|d| d.value == "bank"))
            .expect("bank diff");
        assert_eq!((bank.before, bank.after), (0, 7));
    }
}
