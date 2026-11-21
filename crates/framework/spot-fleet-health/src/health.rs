//! The three-colour health model and the fleet view the console reads.
//!
//! [`Health`] is ordered `Green < Amber < Red`, so "the worst of the findings" is a `max`.
//! Amber means the system is handling the problem automatically or a service is degraded;
//! red means a person is needed. A subject with no findings is green **only** because the
//! evaluator adds a freshness finding when telemetry is missing; this module does not decide
//! that on its own.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::rules::RuleId;
use crate::signal::{NodeId, Signal, Subject, TenantId};
use crate::time::Timestamp;

/// The three colours, ordered from healthy to needing a person.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Everything checked out; telemetry is recent.
    #[default]
    Green,
    /// The system is handling it automatically, or the service is degraded but usable.
    Amber,
    /// Automation is exhausted or damage is possible; a person is needed.
    Red,
}

impl Health {
    /// The colour as a stable lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Amber => "amber",
            Self::Red => "red",
        }
    }

    /// Whether this colour asks for a person.
    #[must_use]
    pub const fn needs_person(self) -> bool {
        matches!(self, Self::Red)
    }

    /// Whether this colour needs attention (amber or red).
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        !matches!(self, Self::Green)
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// One reason a subject is not green: a rule that fired, or the freshness register.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The subject the finding is about.
    pub subject: Subject,
    /// The rule that produced it, or [`RuleId::freshness`] for silence.
    pub rule_id: RuleId,
    /// The signal that breached, or `None` for freshness.
    pub signal: Option<Signal>,
    /// The colour this finding contributes.
    pub health: Health,
    /// The latest breaching value, or `None` for freshness with no reading.
    pub value: Option<i64>,
    /// When the breach began (the first breaching reading in the window).
    pub since: Timestamp,
    /// The latest reading that established the breach.
    pub observed_at: Timestamp,
    /// The plain-language sentence for this finding.
    pub sentence: String,
    /// What a person should do about it.
    pub action: String,
}

impl Finding {
    /// Whether the finding is the freshness register rather than a threshold rule.
    #[must_use]
    pub fn is_freshness(&self) -> bool {
        self.rule_id == RuleId::freshness()
    }
}

/// One subject's health: the worst finding, its sentence, and every finding behind it.
///
/// The sentence is the **worst** finding's sentence, because that is what an operator should
/// read first; the full list is kept for the detail view and for alert grouping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectHealth {
    /// The tenant or node.
    pub subject: Subject,
    /// The worst colour among the findings, green when there are none.
    pub health: Health,
    /// One sentence in plain language describing the worst finding (or that all is well).
    pub sentence: String,
    /// The suggested action for the worst finding, absent when green.
    pub action: Option<String>,
    /// Every finding for this subject, worst first.
    pub findings: Vec<Finding>,
    /// The newest reading for this subject, whatever its signal.
    pub last_observed_at: Option<Timestamp>,
    /// Whether telemetry is older than the freshness threshold.
    pub stale: bool,
    /// Whether this subject has never reported at all.
    pub no_data: bool,
}

impl SubjectHealth {
    /// Whether the subject needs attention (amber or red).
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.health.needs_attention()
    }

    /// The finding behind the displayed sentence, when the subject is not green.
    #[must_use]
    pub fn primary_finding(&self) -> Option<&Finding> {
        self.findings.first()
    }
}

/// Counts by colour, for a dashboard summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthCounts {
    /// Subjects with no findings.
    pub green: usize,
    /// Subjects the system is handling or that are degraded.
    pub amber: usize,
    /// Subjects that need a person.
    pub red: usize,
}

impl HealthCounts {
    /// All subjects counted.
    #[must_use]
    pub const fn total(self) -> usize {
        self.green + self.amber + self.red
    }
}

/// The fleet view: one [`SubjectHealth`] per tenant and per node, at one evaluation instant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHealth {
    /// The instant the evaluation ran for.
    pub generated_at: Timestamp,
    /// One entry per known subject, tenants and nodes in a stable order.
    pub subjects: Vec<SubjectHealth>,
}

impl FleetHealth {
    /// The health of `subject`, when it is in the view.
    #[must_use]
    pub fn get(&self, subject: &Subject) -> Option<&SubjectHealth> {
        self.subjects.iter().find(|entry| &entry.subject == subject)
    }

    /// The health of a tenant, when it is in the view.
    #[must_use]
    pub fn tenant(&self, id: &TenantId) -> Option<&SubjectHealth> {
        self.get(&Subject::Tenant(id.clone()))
    }

    /// The health of a node, when it is in the view.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&SubjectHealth> {
        self.get(&Subject::Node(id.clone()))
    }

    /// The worst colour in the fleet.
    #[must_use]
    pub fn worst(&self) -> Health {
        self.subjects
            .iter()
            .map(|entry| entry.health)
            .max()
            .unwrap_or_default()
    }

    /// Counts by colour.
    #[must_use]
    pub fn counts(&self) -> HealthCounts {
        let mut counts = HealthCounts::default();
        for entry in &self.subjects {
            match entry.health {
                Health::Green => counts.green += 1,
                Health::Amber => counts.amber += 1,
                Health::Red => counts.red += 1,
            }
        }
        counts
    }

    /// Every finding in the view, subject by subject.
    pub fn findings(&self) -> impl Iterator<Item = &Finding> {
        self.subjects.iter().flat_map(|entry| entry.findings.iter())
    }

    /// The subjects that need attention, worst first.
    #[must_use]
    pub fn needs_attention(&self) -> Vec<&SubjectHealth> {
        let mut entries: Vec<&SubjectHealth> = self
            .subjects
            .iter()
            .filter(|entry| entry.needs_attention())
            .collect();
        entries.sort_by(|a, b| {
            b.health
                .cmp(&a.health)
                .then_with(|| a.subject.cmp(&b.subject))
        });
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_order_from_green_to_red() {
        assert!(Health::Green < Health::Amber);
        assert!(Health::Amber < Health::Red);
        assert!(Health::Red.needs_person());
        assert!(!Health::Amber.needs_person());
        assert!(Health::Amber.needs_attention());
    }
}
