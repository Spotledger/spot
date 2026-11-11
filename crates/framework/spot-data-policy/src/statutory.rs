//! Statutory retention floors.
//!
//! A retention rule may delete a record earlier than the law allows. The floor table is the
//! answer: a DocType **class** (for example `financial`) and a **jurisdiction** (for example
//! `de`) map to the period the law requires, as data. The table is empty by default — this crate
//! never hard-codes a jurisdiction's periods — and Foundation/ERP fills it.
//!
//! ```toml
//! [[floor]]
//! doc_class = "financial"
//! jurisdiction = "de"
//! period = "10y"
//! ```
//!
//! The floor only constrains **deletion**. Archiving keeps the record; anonymisation keeps the
//! ledger-relevant fields. A record whose class appears in the table but whose jurisdiction is
//! absent or unknown is **not** deleted by the engine: an unknown statutory position is treated
//! as unsafe, and the plan says so.

use serde::Deserialize;

use crate::calendar::{Calendar, CivilDate, Period};
use crate::error::DataPolicyError;
use crate::record::Record;

/// What the floor table says about one record's deletion date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorLookup {
    /// The record's class is not subject to a statutory floor in this table.
    NotSubject,
    /// The class is subject to a floor, but the record's jurisdiction is missing from the record
    /// or the table. Deleting it is unsafe.
    UnknownJurisdiction,
    /// The earliest instant (Unix milliseconds at midnight UTC) the record may be deleted.
    Earliest(u64),
}

/// One floor entry: a DocType class, a jurisdiction and the period the law requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FloorEntry {
    /// The DocType class (from the DocType metadata).
    pub doc_class: String,
    /// The jurisdiction (a lower-case identifier, such as `de` or `eu`).
    pub jurisdiction: String,
    /// The statutory period from the retention trigger.
    pub period: Period,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorFile {
    #[serde(default)]
    floor: Vec<RawFloor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloor {
    doc_class: String,
    jurisdiction: String,
    period: String,
}

/// The loaded floor table, by DocType class and jurisdiction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatutoryFloor {
    entries: Vec<FloorEntry>,
}

impl StatutoryFloor {
    /// An empty table: no statutory floors are known (the safe default for a deployment that has
    /// not filled one in; deletion is then governed by the retention rule alone).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses and validates the table from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidFloor`] when the text does not parse, a class or jurisdiction is
    /// malformed, a period is invalid, or an entry is repeated.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let file: FloorFile =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidFloor {
                entry: "<file>".to_owned(),
                reason: error.to_string(),
            })?;
        let mut entries = Vec::with_capacity(file.floor.len());
        for raw in file.floor {
            if !valid_ident(&raw.doc_class) || !valid_ident(&raw.jurisdiction) {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", raw.doc_class, raw.jurisdiction),
                    reason: "a class and a jurisdiction are lower-case identifiers".to_owned(),
                });
            }
            let period =
                raw.period
                    .parse::<Period>()
                    .map_err(|reason| DataPolicyError::InvalidFloor {
                        entry: format!("{}.{}", raw.doc_class, raw.jurisdiction),
                        reason,
                    })?;
            if period.count == 0 {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", raw.doc_class, raw.jurisdiction),
                    reason: "a statutory floor is a positive period".to_owned(),
                });
            }
            if entries.iter().any(|entry: &FloorEntry| {
                entry.doc_class == raw.doc_class && entry.jurisdiction == raw.jurisdiction
            }) {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", raw.doc_class, raw.jurisdiction),
                    reason: "the class and jurisdiction are repeated".to_owned(),
                });
            }
            entries.push(FloorEntry {
                doc_class: raw.doc_class,
                jurisdiction: raw.jurisdiction,
                period,
            });
        }
        Ok(Self { entries })
    }

    /// Builds the table from entries, validating that no pair repeats.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidFloor`] on a zero period, a malformed identifier or a repeated
    /// pair.
    pub fn new(entries: Vec<FloorEntry>) -> Result<Self, DataPolicyError> {
        for (index, entry) in entries.iter().enumerate() {
            if !valid_ident(&entry.doc_class) || !valid_ident(&entry.jurisdiction) {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", entry.doc_class, entry.jurisdiction),
                    reason: "a class and a jurisdiction are lower-case identifiers".to_owned(),
                });
            }
            if entry.period.count == 0 {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", entry.doc_class, entry.jurisdiction),
                    reason: "a statutory floor is a positive period".to_owned(),
                });
            }
            if entries[..index].iter().any(|other| {
                other.doc_class == entry.doc_class && other.jurisdiction == entry.jurisdiction
            }) {
                return Err(DataPolicyError::InvalidFloor {
                    entry: format!("{}.{}", entry.doc_class, entry.jurisdiction),
                    reason: "the class and jurisdiction are repeated".to_owned(),
                });
            }
        }
        Ok(Self { entries })
    }

    /// The lookup for a record whose statutory period starts at `trigger` (a [`CivilDate`]).
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the calendar refuses the period.
    pub fn lookup(
        &self,
        record: &Record,
        trigger: CivilDate,
        calendar: &dyn Calendar,
    ) -> Result<FloorLookup, DataPolicyError> {
        let Some(class) = record.doc_class.as_deref() else {
            return Ok(FloorLookup::NotSubject);
        };
        let class_known = self.entries.iter().any(|entry| entry.doc_class == class);
        if !class_known {
            return Ok(FloorLookup::NotSubject);
        }
        let Some(jurisdiction) = record.jurisdiction.as_deref() else {
            return Ok(FloorLookup::UnknownJurisdiction);
        };
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.doc_class == class && entry.jurisdiction == jurisdiction)
        else {
            return Ok(FloorLookup::UnknownJurisdiction);
        };
        let due = calendar.add(trigger, entry.period)?.to_unix_millis()?;
        Ok(FloorLookup::Earliest(due))
    }

    /// The entries, in file order.
    #[must_use]
    pub fn entries(&self) -> &[FloorEntry] {
        &self.entries
    }

    /// How many entries the table has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn valid_ident(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{FloorLookup, StatutoryFloor};
    use crate::calendar::{CivilDate, Gregorian};
    use crate::record::Record;

    fn record(class: Option<&str>, jurisdiction: Option<&str>) -> Record {
        Record {
            id: "invoice-1".to_owned(),
            doctype: "sales_invoice".to_owned(),
            doc_class: class.map(str::to_owned),
            jurisdiction: jurisdiction.map(str::to_owned),
            fields: Default::default(),
            created: 1,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    #[test]
    fn an_empty_table_subjects_nothing() {
        let table = StatutoryFloor::from_toml("").unwrap();
        assert!(table.is_empty());
        assert_eq!(
            table
                .lookup(
                    &record(Some("financial"), Some("de")),
                    CivilDate::new(2020, 1, 1).unwrap(),
                    &Gregorian,
                )
                .unwrap(),
            FloorLookup::NotSubject
        );
    }

    #[test]
    fn a_known_class_and_jurisdiction_gives_the_earliest_date() {
        let table = StatutoryFloor::from_toml(
            "[[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"10y\"\n",
        )
        .unwrap();
        let due = table
            .lookup(
                &record(Some("financial"), Some("de")),
                CivilDate::new(2020, 2, 29).unwrap(),
                &Gregorian,
            )
            .unwrap();
        assert_eq!(
            due,
            FloorLookup::Earliest(
                CivilDate::new(2030, 2, 28)
                    .unwrap()
                    .to_unix_millis()
                    .unwrap()
            )
        );
    }

    #[test]
    fn a_known_class_without_a_jurisdiction_is_unsafe() {
        let table = StatutoryFloor::from_toml(
            "[[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"10y\"\n",
        )
        .unwrap();
        assert_eq!(
            table
                .lookup(
                    &record(Some("financial"), None),
                    CivilDate::new(2020, 1, 1).unwrap(),
                    &Gregorian,
                )
                .unwrap(),
            FloorLookup::UnknownJurisdiction
        );
        assert_eq!(
            table
                .lookup(
                    &record(Some("financial"), Some("fr")),
                    CivilDate::new(2020, 1, 1).unwrap(),
                    &Gregorian,
                )
                .unwrap(),
            FloorLookup::UnknownJurisdiction
        );
    }

    #[test]
    fn duplicate_entries_are_rejected() {
        assert!(
            StatutoryFloor::from_toml(
                "[[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"10y\"\n\
                 [[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"6y\"\n"
            )
            .is_err()
        );
    }
}
