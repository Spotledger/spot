//! The health rules, loaded from `rules/*.toml`.
//!
//! A rule is data: an id, a signal, the subject kind it applies to, a comparison and
//! threshold, how long the breach must last, the severity, the sentence template and the
//! suggested action. The evaluator in [`crate::evaluate`] is a pure function over this data;
//! adding a failure mode is a TOML change, not a code change.
//!
//! # The freshness register
//!
//! A silent subject is not green. [`FreshnessRule`] is the data entry that says how long
//! telemetry may be absent before the subject turns amber, what to say when it was never seen
//! and what a person should do. It is evaluated by the same pure pass as every other rule.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::FleetHealthError;
use crate::health::Health;
use crate::signal::{Signal, Subject, SubjectKind};

/// The identifier of a rule. Lower-case snake case, so it is a stable key but never mistaken
/// for a dotted error code.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RuleId(String);

impl RuleId {
    /// Validates and builds a rule id.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidRuleSet`] when the id is empty, longer than 64 bytes, does
    /// not start with a lower-case letter, or contains anything but lower-case letters,
    /// digits and underscores.
    pub fn try_new(value: impl Into<String>) -> Result<Self, FleetHealthError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err(FleetHealthError::InvalidRuleSet {
                reason: "a rule id must be lower-case snake case, 1 to 64 characters".to_owned(),
            });
        }
        Ok(Self(value))
    }

    /// The id used for the freshness finding.
    #[must_use]
    pub fn freshness() -> Self {
        Self("freshness_telemetry_age".to_owned())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RuleId {
    type Error = FleetHealthError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RuleId> for String {
    fn from(value: RuleId) -> Self {
        value.0
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

/// How a rule compares an observation with its threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// The breach is a value strictly above the threshold.
    Above,
    /// The breach is a value strictly below the threshold.
    Below,
}

impl Comparison {
    /// Whether `value` breaches `threshold` under this comparison.
    #[must_use]
    pub const fn breaches(self, value: i64, threshold: i64) -> bool {
        match self {
            Self::Above => value > threshold,
            Self::Below => value < threshold,
        }
    }

    /// The comparison as it appears in the rule file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

/// One health rule, as `rules/*.toml` carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable rule id.
    pub id: RuleId,
    /// Which signal the rule reads.
    pub signal: Signal,
    /// Whether the rule applies to tenants, nodes or both.
    pub subject: SubjectKind,
    /// Whether a breach is above or below the threshold.
    pub comparison: Comparison,
    /// The threshold, in the signal's own unit.
    pub threshold: i64,
    /// How long the breach must have lasted, in seconds. Zero fires on the first reading.
    pub for_seconds: u64,
    /// Amber (handled automatically or degraded) or red (a person is needed).
    pub severity: Health,
    /// Sentence template. `{subject}`, `{value}`, `{signal}` and `{duration}` are replaced.
    pub sentence: String,
    /// What a person should do about it.
    pub action: String,
}

impl Rule {
    /// Whether this rule applies to `subject`.
    #[must_use]
    pub const fn applies_to(&self, subject: &Subject) -> bool {
        matches!(
            (self.subject, subject.kind()),
            (SubjectKind::Either, _)
                | (SubjectKind::Tenant, SubjectKind::Tenant)
                | (SubjectKind::Node, SubjectKind::Node)
        )
    }

    /// Whether `value` breaches this rule's threshold.
    #[must_use]
    pub const fn breaches(&self, value: i64) -> bool {
        self.comparison.breaches(value, self.threshold)
    }
}

/// The freshness data entry: when silence itself turns a subject amber.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshnessRule {
    /// Telemetry older than this many seconds turns the subject amber.
    pub after_seconds: u64,
    /// Sentence template for stale telemetry (`{subject}` and `{duration}`).
    pub sentence: String,
    /// Sentence for a subject that has never reported at all (`{subject}`).
    pub no_data_sentence: String,
    /// What a person should do when a subject is silent.
    pub action: String,
}

/// The complete rule set: the threshold rules plus the freshness register.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSet {
    /// The threshold rules, in file order.
    pub rules: Vec<Rule>,
    /// The freshness register.
    pub freshness: FreshnessRule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuleSet {
    #[serde(default)]
    rule: Vec<Rule>,
    freshness: Option<FreshnessRule>,
}

impl RuleSet {
    /// The shipped rules (`rules/signals.toml` and `rules/freshness.toml`), compiled in.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidRuleSet`] when the compiled-in files do not parse or violate
    /// an invariant. A build that reaches this state is a defect, but the caller still gets a
    /// typed answer rather than a panic.
    pub fn builtin() -> Result<Self, FleetHealthError> {
        Self::parse(&[
            include_str!("../rules/signals.toml"),
            include_str!("../rules/freshness.toml"),
        ])
    }

    /// Parses and validates one or more rule files. `[[rule]]` tables are concatenated; the
    /// `[freshness]` table must appear exactly once across the inputs.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidRuleSet`] naming the first problem found: a TOML parse
    /// error, a missing or duplicate freshness register, a duplicate rule id, an invalid rule
    /// id, an empty sentence or action, a threshold outside the signal's domain, or a green
    /// severity (green is the absence of findings, not a rule).
    pub fn parse(files: &[&str]) -> Result<Self, FleetHealthError> {
        let mut rules = Vec::new();
        let mut freshness: Option<FreshnessRule> = None;
        for (index, text) in files.iter().enumerate() {
            let raw: RawRuleSet =
                toml::from_str(text).map_err(|error| FleetHealthError::InvalidRuleSet {
                    reason: format!("rule file {}: {error}", index + 1),
                })?;
            rules.extend(raw.rule);
            if let Some(next) = raw.freshness {
                if freshness.is_some() {
                    return Err(FleetHealthError::InvalidRuleSet {
                        reason: "the freshness register appears more than once".to_owned(),
                    });
                }
                freshness = Some(next);
            }
        }
        let freshness = freshness.ok_or_else(|| FleetHealthError::InvalidRuleSet {
            reason: "the freshness register is missing".to_owned(),
        })?;
        let set = Self { rules, freshness };
        set.validate()?;
        Ok(set)
    }

    /// Checks the invariants every rule set must satisfy.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidRuleSet`] naming the first problem.
    pub fn validate(&self) -> Result<(), FleetHealthError> {
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.rules {
            if !seen.insert(rule.id.clone()) {
                return Err(FleetHealthError::InvalidRuleSet {
                    reason: format!("rule id `{}` appears more than once", rule.id),
                });
            }
            if rule.severity == Health::Green {
                return Err(FleetHealthError::InvalidRuleSet {
                    reason: format!("rule `{}` cannot be green", rule.id),
                });
            }
            if rule.sentence.trim().is_empty() || rule.action.trim().is_empty() {
                return Err(FleetHealthError::InvalidRuleSet {
                    reason: format!("rule `{}` needs a sentence and an action", rule.id),
                });
            }
            if !rule.signal.accepts(rule.threshold) {
                return Err(FleetHealthError::InvalidRuleSet {
                    reason: format!(
                        "rule `{}` has threshold {} outside the {} domain",
                        rule.id, rule.threshold, rule.signal
                    ),
                });
            }
        }
        if self.freshness.after_seconds == 0 {
            return Err(FleetHealthError::InvalidRuleSet {
                reason: "the freshness threshold must be at least one second".to_owned(),
            });
        }
        if self.freshness.sentence.trim().is_empty()
            || self.freshness.no_data_sentence.trim().is_empty()
            || self.freshness.action.trim().is_empty()
        {
            return Err(FleetHealthError::InvalidRuleSet {
                reason: "the freshness register needs both sentences and an action".to_owned(),
            });
        }
        Ok(())
    }

    /// The rules, in file order.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The rule named `id`, when it exists.
    #[must_use]
    pub fn rule(&self, id: &RuleId) -> Option<&Rule> {
        self.rules.iter().find(|rule| &rule.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_rules_load_and_cover_every_signal() {
        let rules = RuleSet::builtin().expect("builtin rules");
        assert!(rules.rules.len() >= 15);
        for signal in Signal::ALL {
            assert!(
                rules.rules.iter().any(|rule| rule.signal == signal),
                "a rule covers {signal}"
            );
        }
        assert_eq!(rules.freshness.after_seconds, 45);
    }

    #[test]
    fn a_duplicate_rule_id_is_refused() {
        let toml = r#"
            [[rule]]
            id = "same"
            signal = "queue_lag"
            subject = "tenant"
            comparison = "above"
            threshold = 10
            for_seconds = 0
            severity = "amber"
            sentence = "Queue lag for {subject}."
            action = "Wait."

            [[rule]]
            id = "same"
            signal = "disk_usage"
            subject = "tenant"
            comparison = "above"
            threshold = 10
            for_seconds = 0
            severity = "amber"
            sentence = "Disk for {subject}."
            action = "Wait."

            [freshness]
            after_seconds = 45
            sentence = "No data from {subject} for {duration}."
            no_data_sentence = "No data from {subject}."
            action = "Check it."
        "#;
        assert!(RuleSet::parse(&[toml]).is_err());
    }

    #[test]
    fn a_green_severity_is_refused() {
        let toml = r#"
            [[rule]]
            id = "green_rule"
            signal = "queue_lag"
            subject = "tenant"
            comparison = "above"
            threshold = 10
            for_seconds = 0
            severity = "green"
            sentence = "Queue lag for {subject}."
            action = "Wait."

            [freshness]
            after_seconds = 45
            sentence = "No data from {subject} for {duration}."
            no_data_sentence = "No data from {subject}."
            action = "Check it."
        "#;
        assert!(RuleSet::parse(&[toml]).is_err());
    }
}
