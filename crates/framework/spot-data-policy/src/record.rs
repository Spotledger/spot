//! The record shape retention reasons about, and the field classification anonymisation uses.
//!
//! Retention does not know DocTypes or their meaning; it operates on the small, typed view a
//! document store hands it. Everything is integer or text — no floats (ADR-12) — and a record is
//! data, never a live document handle.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::calendar::CivilDate;
use crate::error::DataPolicyError;
use crate::rule::Trigger;

/// A field value in the retention view. There is deliberately no float variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldValue {
    /// Text.
    Text(String),
    /// A signed integer.
    Integer(i64),
    /// A monetary amount in minor units (cents). Never a float.
    Money(u64),
    /// A boolean.
    Bool(bool),
    /// A date at midnight UTC, in Unix milliseconds.
    Date(u64),
}

impl FieldValue {
    /// Whether a value is a money or quantity total that must survive anonymisation.
    #[must_use]
    pub fn is_total(&self) -> bool {
        matches!(self, Self::Money(_))
    }
}

/// The classification of a field from the DocType metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// May be shown and kept.
    Public,
    /// Internal but not personal.
    Internal,
    /// Personal data: anonymised when a record is anonymised (unless ledger-relevant).
    Pii,
    /// A secret or special-category value: always anonymised when the record is.
    Sensitive,
}

impl Classification {
    /// Whether the classification is personal data that anonymisation must replace.
    #[must_use]
    pub fn is_personal(self) -> bool {
        matches!(self, Self::Pii | Self::Sensitive)
    }
}

/// What the metadata says about one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// The field's classification.
    pub classification: Classification,
    /// Whether the field is needed to keep a ledger total intelligible. A ledger-relevant field is
    /// kept even when it is PII (for example the counterparty on a posted entry), because deleting
    /// it would break the audit trail the law requires.
    #[serde(default)]
    pub ledger_relevant: bool,
}

impl FieldSpec {
    /// A classified field that is not ledger-relevant.
    #[must_use]
    pub fn new(classification: Classification) -> Self {
        Self {
            classification,
            ledger_relevant: false,
        }
    }

    /// The same field, marked ledger-relevant.
    #[must_use]
    pub fn ledger(self) -> Self {
        Self {
            ledger_relevant: true,
            ..self
        }
    }

    /// Whether anonymisation replaces this field.
    #[must_use]
    pub fn anonymise(self) -> bool {
        self.classification.is_personal() && !self.ledger_relevant
    }
}

/// The field classification table: DocType -> field -> [`FieldSpec`], as metadata text.
///
/// ```toml
/// [[field]]
/// doctype = "invoice"
/// name = "customer_name"
/// classification = "pii"
///
/// [[field]]
/// doctype = "invoice"
/// name = "total"
/// classification = "internal"
/// ledger_relevant = true
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldClassMap {
    fields: BTreeMap<String, BTreeMap<String, FieldSpec>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldFile {
    #[serde(default)]
    field: Vec<FieldEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldEntry {
    doctype: String,
    name: String,
    classification: Classification,
    #[serde(default)]
    ledger_relevant: bool,
}

impl FieldClassMap {
    /// Parses the table from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidClassification`] when the text does not parse, a name is empty or
    /// a field is repeated.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let file: FieldFile =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidClassification {
                entry: "<file>".to_owned(),
                reason: error.to_string(),
            })?;
        let mut fields: BTreeMap<String, BTreeMap<String, FieldSpec>> = BTreeMap::new();
        for entry in file.field {
            if !valid_name(&entry.doctype) || !valid_name(&entry.name) {
                return Err(DataPolicyError::InvalidClassification {
                    entry: format!("{}.{}", entry.doctype, entry.name),
                    reason: "a DocType and a field name are lower-case identifiers".to_owned(),
                });
            }
            let spec = FieldSpec {
                classification: entry.classification,
                ledger_relevant: entry.ledger_relevant,
            };
            let doctype = fields.entry(entry.doctype.clone()).or_default();
            if doctype.insert(entry.name.clone(), spec).is_some() {
                return Err(DataPolicyError::InvalidClassification {
                    entry: format!("{}.{}", entry.doctype, entry.name),
                    reason: "the field is classified twice".to_owned(),
                });
            }
        }
        Ok(Self { fields })
    }

    /// The spec of one field, if the metadata classifies it.
    #[must_use]
    pub fn spec(&self, doctype: &str, field: &str) -> Option<FieldSpec> {
        self.fields
            .get(doctype)
            .and_then(|fields| fields.get(field))
            .copied()
    }

    /// The fields of `doctype` that anonymisation replaces, in stable name order.
    #[must_use]
    pub fn anonymise_fields(&self, doctype: &str) -> Vec<String> {
        self.fields
            .get(doctype)
            .map(|fields| {
                fields
                    .iter()
                    .filter(|(_, spec)| spec.anonymise())
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the table classifies any field of `doctype`.
    #[must_use]
    pub fn knows(&self, doctype: &str) -> bool {
        self.fields.contains_key(doctype)
    }

    /// Every classification, in stable (DocType, field) order — the canonical input to a plan's
    /// identity digest.
    #[must_use]
    pub fn entries(&self) -> Vec<(&str, &str, FieldSpec)> {
        let mut out = Vec::new();
        for (doctype, fields) in &self.fields {
            for (field, spec) in fields {
                out.push((doctype.as_str(), field.as_str(), *spec));
            }
        }
        out
    }
}

fn valid_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// One record as retention sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The stable document id.
    pub id: String,
    /// The DocType name (for example `sales_invoice`).
    pub doctype: String,
    /// The statutory class from the DocType metadata (for example `financial`), if it has one.
    #[serde(default)]
    pub doc_class: Option<String>,
    /// The jurisdiction from the DocType metadata (for example `de`), if it has one.
    #[serde(default)]
    pub jurisdiction: Option<String>,
    /// The record's fields, keyed by field name.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldValue>,
    /// When the record was created, in Unix milliseconds.
    pub created: u64,
    /// When the record was closed, in Unix milliseconds, if it has a closed trigger.
    #[serde(default)]
    pub closed: Option<u64>,
    /// Business event dates the record carries, keyed by event name.
    #[serde(default)]
    pub events: BTreeMap<String, u64>,
    /// Other records this record still links to, by id. Retention never deletes a record another
    /// retained record still links to.
    #[serde(default)]
    pub references: Vec<String>,
}

impl Record {
    /// Checks the record is well formed for retention.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRecord`] when an id, DocType or class is malformed, or a date is
    /// zero where a date is required.
    pub fn validate(&self) -> Result<(), DataPolicyError> {
        if !valid_id(&self.id) {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "an id is 1 to 256 characters".to_owned(),
            });
        }
        if !valid_name(&self.doctype) {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "a DocType is a lower-case identifier".to_owned(),
            });
        }
        if self.doc_class.as_deref().is_some_and(|c| !valid_name(c)) {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "a DocType class is a lower-case identifier".to_owned(),
            });
        }
        if self.jurisdiction.as_deref().is_some_and(|j| !valid_name(j)) {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "a jurisdiction is a lower-case identifier".to_owned(),
            });
        }
        if self.created == 0 {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "a record has a creation instant".to_owned(),
            });
        }
        if self.references.iter().any(|r| !valid_id(r)) {
            return Err(DataPolicyError::InvalidRecord {
                record: self.id.clone(),
                reason: "a reference is 1 to 256 characters".to_owned(),
            });
        }
        Ok(())
    }

    /// The instant a rule trigger starts from, if the record carries it.
    #[must_use]
    pub fn trigger_at(&self, trigger: &Trigger) -> Option<u64> {
        match trigger {
            Trigger::Created => Some(self.created),
            Trigger::Closed => self.closed,
            Trigger::Event(name) => self.events.get(name).copied(),
        }
    }

    /// The trigger instant as a calendar date.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the instant is out of the calendar's range.
    pub fn trigger_date(&self, trigger: &Trigger) -> Result<Option<CivilDate>, DataPolicyError> {
        match self.trigger_at(trigger) {
            None => Ok(None),
            Some(at) => CivilDate::from_unix_millis(at).map(Some),
        }
    }

    /// Whether this record links to `id`.
    #[must_use]
    pub fn references_record(&self, id: &str) -> bool {
        self.references.iter().any(|reference| reference == id)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Classification, FieldClassMap, FieldSpec, FieldValue};
    use crate::rule::Trigger;

    #[test]
    fn classification_decides_what_anonymisation_replaces() {
        assert!(FieldSpec::new(Classification::Pii).anonymise());
        assert!(FieldSpec::new(Classification::Sensitive).anonymise());
        assert!(!FieldSpec::new(Classification::Public).anonymise());
        assert!(!FieldSpec::new(Classification::Pii).ledger().anonymise());
        assert!(FieldValue::Money(100).is_total());
        assert!(!FieldValue::Integer(100).is_total());
    }

    #[test]
    fn the_field_map_lists_only_personal_non_ledger_fields() {
        let map = FieldClassMap::from_toml(
            "[[field]]\ndoctype = \"invoice\"\nname = \"customer_name\"\nclassification = \"pii\"\n\
             [[field]]\ndoctype = \"invoice\"\nname = \"customer_ref\"\nclassification = \"pii\"\nledger_relevant = true\n\
             [[field]]\ndoctype = \"invoice\"\nname = \"total\"\nclassification = \"internal\"\nledger_relevant = true\n",
        )
        .unwrap();
        assert_eq!(map.anonymise_fields("invoice"), vec!["customer_name"]);
        assert!(map.knows("invoice"));
        assert!(!map.knows("other"));
    }

    #[test]
    fn duplicate_classifications_are_rejected() {
        assert!(
            FieldClassMap::from_toml(
                "[[field]]\ndoctype = \"invoice\"\nname = \"total\"\nclassification = \"internal\"\n\
                 [[field]]\ndoctype = \"invoice\"\nname = \"total\"\nclassification = \"public\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn a_trigger_date_uses_the_named_event() {
        let date = RecordDate::at(2026, 10, 19);
        let record = super::Record {
            id: "x".to_owned(),
            doctype: "invoice".to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: Default::default(),
            created: date,
            closed: Some(date + 1),
            events: [("paid".to_owned(), date + 2)].into(),
            references: Vec::new(),
        };
        assert_eq!(record.trigger_at(&Trigger::Created), Some(date));
        assert_eq!(record.trigger_at(&Trigger::Closed), Some(date + 1));
        assert_eq!(
            record.trigger_at(&Trigger::Event("paid".to_owned())),
            Some(date + 2)
        );
        assert_eq!(record.trigger_at(&Trigger::Event("void".to_owned())), None);
        assert!(record.validate().is_ok());
    }

    struct RecordDate;

    impl RecordDate {
        fn at(year: i32, month: u8, day: u8) -> u64 {
            crate::calendar::CivilDate::new(year, month, day)
                .unwrap()
                .to_unix_millis()
                .unwrap()
        }
    }
}
