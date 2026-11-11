//! Retention rules in metadata text form.
//!
//! A rule says: when a record of `doctype` reaches `trigger`, `period` later, `action` applies,
//! except for the records the rule's exceptions name. Rules are metadata (TOML), owned by the
//! app or the tenant, never a jurisdiction's periods hard-coded in this crate (ADR-12 and the
//! task's "must not").
//!
//! ```toml
//! [[rule]]
//! id = "invoices-10y"
//! doctype = "sales_invoice"
//! trigger = "closed"
//! period = "10y"
//! action = "archive"
//!
//! [[rule]]
//! id = "drafts-90d"
//! doctype = "sales_draft"
//! trigger = "created"
//! period = "90d"
//! action = "delete"
//! exceptions = { doc_classes = ["financial"], record_ids = ["draft-keep-1"] }
//! ```
//!
//! One rule per DocType: two schedules for the same DocType would race the planner, so the loader
//! refuses them. A DocType may have several rules only after the plan defines how they compose.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::DataPolicyError;
use crate::record::Record;

/// What starts a retention period.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trigger {
    /// The record's creation instant.
    Created,
    /// The instant the record was closed (posted, cancelled, completed).
    Closed,
    /// A named business event the record carries (`event:<name>`).
    Event(String),
}

impl Trigger {
    /// The trigger in metadata text form.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Created => "created".to_owned(),
            Self::Closed => "closed".to_owned(),
            Self::Event(name) => format!("event:{name}"),
        }
    }
}

impl FromStr for Trigger {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "created" => Ok(Self::Created),
            "closed" => Ok(Self::Closed),
            other => match other.strip_prefix("event:") {
                Some(name) if valid_event(name) => Ok(Self::Event(name.to_owned())),
                Some(_) => {
                    Err("an event trigger names an event like event:invoice.paid".to_owned())
                }
                None => Err("a trigger is created, closed or event:<name>".to_owned()),
            },
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_text())
    }
}

fn valid_event(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

/// What retention does to a record that reaches its due date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// Delete the record (through the document lifecycle), keeping a deletion proof.
    Delete,
    /// Replace every personal, non-ledger field through the anonymise operation.
    Anonymise,
    /// Move the record to the archive store; the record is still retained.
    Archive,
}

impl Action {
    /// The action in metadata text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Anonymise => "anonymise",
            Self::Archive => "archive",
        }
    }
}

impl FromStr for Action {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "delete" => Ok(Self::Delete),
            "anonymise" => Ok(Self::Anonymise),
            "archive" => Ok(Self::Archive),
            _ => Err("an action is delete, anonymise or archive".to_owned()),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Records excluded from a rule even when they are due.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exceptions {
    /// Specific record ids the rule never actions.
    #[serde(default)]
    pub record_ids: Vec<String>,
    /// DocType classes the rule never actions (for example records a statutory rule owns).
    #[serde(default)]
    pub doc_classes: Vec<String>,
}

impl Exceptions {
    /// Whether the rule must leave `record` alone.
    #[must_use]
    pub fn applies(&self, record: &Record) -> bool {
        if self.record_ids.iter().any(|id| id == &record.id) {
            return true;
        }
        match &record.doc_class {
            Some(class) => self.doc_classes.iter().any(|c| c == class),
            None => false,
        }
    }
}

/// One retention rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionRule {
    /// Stable rule id, unique within the rule set and written into every outcome and proof.
    pub id: String,
    /// The DocType the rule applies to.
    pub doctype: String,
    /// What starts the period.
    pub trigger: Trigger,
    /// How long the record is retained after the trigger, calendar-aware.
    pub period: crate::calendar::Period,
    /// What happens when the period ends.
    pub action: Action,
    /// Records the rule never actions.
    pub exceptions: Exceptions,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    doctype: String,
    trigger: String,
    period: String,
    action: String,
    #[serde(default)]
    exceptions: Exceptions,
}

/// A loaded, validated rule set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionRules {
    rules: Vec<RetentionRule>,
}

impl RetentionRules {
    /// An empty rule set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses and validates the rule set from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRule`] when the text does not parse, a rule is malformed, two
    /// rules share an id, or two rules name one DocType.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let file: RuleFile =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidRule {
                rule: "<file>".to_owned(),
                reason: error.to_string(),
            })?;
        let mut rules = Vec::with_capacity(file.rule.len());
        for raw in file.rule {
            let rule = build_rule(raw)?;
            rules.push(rule);
        }
        let rules = Self { rules };
        rules.validate()?;
        Ok(rules)
    }

    /// Builds a rule set from already-constructed rules, validating the same invariants.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRule`] on an empty id, a malformed DocType, a duplicate id or a
    /// second rule for one DocType.
    pub fn new(rules: Vec<RetentionRule>) -> Result<Self, DataPolicyError> {
        let set = Self { rules };
        set.validate()?;
        Ok(set)
    }

    fn validate(&self) -> Result<(), DataPolicyError> {
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.id.trim().is_empty() || rule.id.len() > 128 {
                return Err(DataPolicyError::InvalidRule {
                    rule: format!("<rule {index}>"),
                    reason: "an id has 1 to 128 characters".to_owned(),
                });
            }
            if !valid_doctype(&rule.doctype) {
                return Err(DataPolicyError::InvalidRule {
                    rule: rule.id.clone(),
                    reason: "a DocType is a lower-case identifier".to_owned(),
                });
            }
            if self.rules[..index].iter().any(|other| other.id == rule.id) {
                return Err(DataPolicyError::InvalidRule {
                    rule: rule.id.clone(),
                    reason: "the rule id is repeated".to_owned(),
                });
            }
            if self.rules[..index]
                .iter()
                .any(|other| other.doctype == rule.doctype)
            {
                return Err(DataPolicyError::InvalidRule {
                    rule: rule.id.clone(),
                    reason: "the DocType already has a rule".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// The rules in file order.
    #[must_use]
    pub fn rules(&self) -> &[RetentionRule] {
        &self.rules
    }

    /// The rule with `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&RetentionRule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// The rule for `doctype`.
    #[must_use]
    pub fn for_doctype(&self, doctype: &str) -> Option<&RetentionRule> {
        self.rules.iter().find(|rule| rule.doctype == doctype)
    }

    /// How many rules the set has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

fn valid_doctype(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn build_rule(raw: RawRule) -> Result<RetentionRule, DataPolicyError> {
    let trigger =
        Trigger::from_str(&raw.trigger).map_err(|reason| DataPolicyError::InvalidRule {
            rule: raw.id.clone(),
            reason,
        })?;
    let period = raw
        .period
        .parse::<crate::calendar::Period>()
        .map_err(|reason| DataPolicyError::InvalidRule {
            rule: raw.id.clone(),
            reason,
        })?;
    let action = Action::from_str(&raw.action).map_err(|reason| DataPolicyError::InvalidRule {
        rule: raw.id.clone(),
        reason,
    })?;
    Ok(RetentionRule {
        id: raw.id,
        doctype: raw.doctype,
        trigger,
        period,
        action,
        exceptions: raw.exceptions,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Action, Exceptions, RetentionRule, RetentionRules, Trigger};
    use crate::calendar::Period;

    fn rules() -> RetentionRules {
        RetentionRules::from_toml(
            "[[rule]]\nid = \"a\"\ndoctype = \"invoice\"\ntrigger = \"closed\"\nperiod = \"10y\"\naction = \"archive\"\n\
             [[rule]]\nid = \"b\"\ndoctype = \"draft\"\ntrigger = \"event:invoice.paid\"\nperiod = \"90d\"\naction = \"delete\"\n",
        )
        .unwrap()
    }

    #[test]
    fn rules_parse_into_the_typed_form() {
        let set = rules();
        assert_eq!(set.len(), 2);
        let rule = set.get("a").unwrap();
        assert_eq!(rule.trigger, Trigger::Closed);
        assert_eq!(rule.period, Period::years(10));
        assert_eq!(rule.action, Action::Archive);
        assert_eq!(
            set.get("b").unwrap().trigger,
            Trigger::Event("invoice.paid".to_owned())
        );
    }

    #[test]
    fn one_doc_type_cannot_have_two_rules() {
        assert!(
            RetentionRules::from_toml(
                "[[rule]]\nid = \"a\"\ndoctype = \"invoice\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n\
                 [[rule]]\nid = \"b\"\ndoctype = \"invoice\"\ntrigger = \"closed\"\nperiod = \"2y\"\naction = \"archive\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        assert!(
            RetentionRules::from_toml(
                "[[rule]]\nid = \"a\"\ndoctype = \"invoice\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n\
                 [[rule]]\nid = \"a\"\ndoctype = \"draft\"\ntrigger = \"created\"\nperiod = \"2y\"\naction = \"delete\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn exceptions_match_record_ids_and_classes() {
        let exceptions = Exceptions {
            record_ids: vec!["keep".to_owned()],
            doc_classes: vec!["financial".to_owned()],
        };
        let record = super::tests::record("keep", None);
        assert!(exceptions.applies(&record));
        let record = super::tests::record("other", Some("financial"));
        assert!(exceptions.applies(&record));
        let record = super::tests::record("other", Some("operational"));
        assert!(!exceptions.applies(&record));
    }

    #[test]
    fn a_rule_can_be_built_in_code() {
        let rule = RetentionRule {
            id: "x".to_owned(),
            doctype: "note".to_owned(),
            trigger: Trigger::Created,
            period: Period::days(30),
            action: Action::Delete,
            exceptions: Exceptions::default(),
        };
        assert_eq!(RetentionRules::new(vec![rule]).unwrap().len(), 1);
    }

    fn record(id: &str, class: Option<&str>) -> crate::record::Record {
        crate::record::Record {
            id: id.to_owned(),
            doctype: "invoice".to_owned(),
            doc_class: class.map(str::to_owned),
            jurisdiction: None,
            fields: Default::default(),
            created: 1,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }
}
