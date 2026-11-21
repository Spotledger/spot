//! Legal holds.
//!
//! A hold suspends **every** retention action for the records it covers, whatever their rule and
//! however long they have been due (a litigation hold, an investigation, a customer request). A
//! hold is not a rule exception: exceptions exclude a record from one rule, a hold overrides the
//! whole engine until it is lifted. Holds are data (metadata text) and carry who set them and why,
//! so the plan can say exactly why a record was left alone.
//!
//! ```toml
//! [[hold]]
//! id = "litigation-2026-04"
//! reason = "Case 2026-0412: preserve all correspondence"
//! set_by = "user:legal-1"
//! scope = { doctype = "mail_message" }
//! ```

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

use crate::error::DataPolicyError;
use crate::record::Record;

/// The records a hold covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldScope {
    /// Every record of the tenant.
    All,
    /// Every record of one DocType.
    DocType(String),
    /// The named records.
    Records(Vec<String>),
}

impl HoldScope {
    /// Whether the scope covers `record`.
    #[must_use]
    pub fn covers(&self, record: &Record) -> bool {
        match self {
            Self::All => true,
            Self::DocType(doctype) => &record.doctype == doctype,
            Self::Records(ids) => ids.iter().any(|id| id == &record.id),
        }
    }

    /// The scope in metadata text form.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::All => "all".to_owned(),
            Self::DocType(doctype) => format!("doctype:{doctype}"),
            Self::Records(ids) => format!("records:{}", ids.join(",")),
        }
    }
}

impl fmt::Display for HoldScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_text())
    }
}

/// One legal hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    /// Stable hold id, written into the plan's reason.
    pub id: String,
    /// The records it covers.
    pub scope: HoldScope,
    /// Why it was set (the case reference, the request); never a secret.
    pub reason: String,
    /// Who set it (`user:<id>` or a service identity).
    pub set_by: String,
}

impl Hold {
    /// Checks the hold is well formed.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidHold`] when an id or `set_by` is empty, an id is too long, or a
    /// record scope is empty.
    pub fn validate(&self) -> Result<(), DataPolicyError> {
        let reason = if self.id.trim().is_empty() || self.id.len() > 128 {
            Some("an id has 1 to 128 characters")
        } else if self.set_by.trim().is_empty() || self.set_by.len() > 256 {
            Some("set_by names who set the hold")
        } else if self.reason.trim().is_empty() || self.reason.len() > 1_024 {
            Some("a reason has 1 to 1024 characters")
        } else if let HoldScope::Records(ids) = &self.scope {
            if ids.is_empty() || ids.iter().any(|id| id.trim().is_empty() || id.len() > 256) {
                Some("a record scope names at least one record")
            } else {
                None
            }
        } else if let HoldScope::DocType(doctype) = &self.scope {
            if doctype.trim().is_empty() {
                Some("a DocType scope names a DocType")
            } else {
                None
            }
        } else {
            None
        };
        match reason {
            Some(reason) => Err(DataPolicyError::InvalidHold {
                hold: self.id.clone(),
                reason: reason.to_owned(),
            }),
            None => Ok(()),
        }
    }
}

/// A loaded hold set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holds {
    holds: Vec<Hold>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldFile {
    #[serde(default)]
    hold: Vec<RawHold>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHold {
    id: String,
    reason: String,
    set_by: String,
    scope: RawScope,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawScope {
    Named(String),
    Table(ScopeTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeTable {
    #[serde(default)]
    doctype: Option<String>,
    #[serde(default)]
    records: Option<Vec<String>>,
}

impl Holds {
    /// No holds.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses and validates the hold set from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidHold`] when the text does not parse, a hold is malformed, a scope
    /// names both a DocType and records, or an id is repeated.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let file: HoldFile =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidHold {
                hold: "<file>".to_owned(),
                reason: error.to_string(),
            })?;
        let mut holds = Vec::with_capacity(file.hold.len());
        for raw in file.hold {
            let hold = build_hold(raw)?;
            hold.validate()?;
            if holds.iter().any(|other: &Hold| other.id == hold.id) {
                return Err(DataPolicyError::InvalidHold {
                    hold: hold.id,
                    reason: "the hold id is repeated".to_owned(),
                });
            }
            holds.push(hold);
        }
        Ok(Self { holds })
    }

    /// Builds a hold set from already-constructed holds.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidHold`] when a hold is malformed or an id is repeated.
    pub fn new(holds: Vec<Hold>) -> Result<Self, DataPolicyError> {
        for hold in &holds {
            hold.validate()?;
        }
        for (index, hold) in holds.iter().enumerate() {
            if holds[..index].iter().any(|other| other.id == hold.id) {
                return Err(DataPolicyError::InvalidHold {
                    hold: hold.id.clone(),
                    reason: "the hold id is repeated".to_owned(),
                });
            }
        }
        Ok(Self { holds })
    }

    /// The first hold covering `record`, if any.
    #[must_use]
    pub fn covering(&self, record: &Record) -> Option<&Hold> {
        self.holds.iter().find(|hold| hold.scope.covers(record))
    }

    /// Every hold, in file order.
    #[must_use]
    pub fn holds(&self) -> &[Hold] {
        &self.holds
    }

    /// How many holds the set has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.holds.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty()
    }
}

fn build_hold(raw: RawHold) -> Result<Hold, DataPolicyError> {
    let scope = match raw.scope {
        RawScope::Named(name) => match name.trim() {
            "all" => HoldScope::All,
            other => match other.strip_prefix("doctype:") {
                Some(doctype) if !doctype.is_empty() => HoldScope::DocType(doctype.to_owned()),
                Some(_) => {
                    return Err(DataPolicyError::InvalidHold {
                        hold: raw.id,
                        reason: "a doctype: scope names a DocType".to_owned(),
                    });
                }
                None => {
                    return Err(DataPolicyError::InvalidHold {
                        hold: raw.id,
                        reason: "a scope is all, doctype:<name> or a record list".to_owned(),
                    });
                }
            },
        },
        RawScope::Table(table) => match (table.doctype, table.records) {
            (Some(doctype), None) if !doctype.is_empty() => HoldScope::DocType(doctype),
            (None, Some(records)) if !records.is_empty() => HoldScope::Records(records),
            (Some(_), Some(_)) => {
                return Err(DataPolicyError::InvalidHold {
                    hold: raw.id,
                    reason: "a scope names a DocType or records, not both".to_owned(),
                });
            }
            _ => {
                return Err(DataPolicyError::InvalidHold {
                    hold: raw.id,
                    reason: "a scope names a DocType or a non-empty record list".to_owned(),
                });
            }
        },
    };
    Ok(Hold {
        id: raw.id,
        scope,
        reason: raw.reason,
        set_by: raw.set_by,
    })
}

impl FromStr for HoldScope {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text == "all" {
            return Ok(Self::All);
        }
        if let Some(doctype) = text.strip_prefix("doctype:") {
            if doctype.is_empty() {
                return Err("a doctype scope names a DocType".to_owned());
            }
            return Ok(Self::DocType(doctype.to_owned()));
        }
        if let Some(records) = text.strip_prefix("records:") {
            let ids: Vec<String> = records
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
            if ids.is_empty() {
                return Err("a record scope names at least one record".to_owned());
            }
            return Ok(Self::Records(ids));
        }
        Err("a scope is all, doctype:<name> or records:<id>,<id>".to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Hold, HoldScope, Holds};

    fn record(id: &str, doctype: &str) -> crate::record::Record {
        crate::record::Record {
            id: id.to_owned(),
            doctype: doctype.to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: Default::default(),
            created: 1,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    #[test]
    fn scopes_cover_what_they_name() {
        let all = Holds::from_toml(
            "[[hold]]\nid = \"h\"\nreason = \"case\"\nset_by = \"user:legal\"\nscope = \"all\"\n",
        )
        .unwrap();
        assert!(all.covering(&record("a", "invoice")).is_some());

        let set = Holds::from_toml(
            "[[hold]]\nid = \"h1\"\nreason = \"case\"\nset_by = \"user:legal\"\nscope = { doctype = \"mail\" }\n\
             [[hold]]\nid = \"h2\"\nreason = \"case\"\nset_by = \"user:legal\"\nscope = { records = [\"invoice-7\"] }\n",
        )
        .unwrap();
        assert!(set.covering(&record("a", "mail")).is_some());
        assert!(set.covering(&record("invoice-7", "invoice")).is_some());
        assert!(set.covering(&record("invoice-8", "invoice")).is_none());
    }

    #[test]
    fn a_scope_cannot_name_both() {
        assert!(
            Holds::from_toml(
                "[[hold]]\nid = \"h\"\nreason = \"case\"\nset_by = \"user:legal\"\nscope = { doctype = \"mail\", records = [\"a\"] }\n"
            )
            .is_err()
        );
    }

    #[test]
    fn a_hold_needs_a_reason_and_who_set_it() {
        let hold = Hold {
            id: "h".to_owned(),
            scope: HoldScope::All,
            reason: " ".to_owned(),
            set_by: "user:legal".to_owned(),
        };
        assert!(hold.validate().is_err());
        let hold = Hold {
            id: "h".to_owned(),
            scope: HoldScope::All,
            reason: "case".to_owned(),
            set_by: String::new(),
        };
        assert!(hold.validate().is_err());
    }
}
