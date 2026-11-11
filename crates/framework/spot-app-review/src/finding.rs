//! Findings: one problem a reviewer should read, with the rule that raised it.

use serde::{Deserialize, Serialize};

/// How serious a finding is. `Blocker` rejects the bundle outright; `High` and above mean a
/// person must decide; the review never approves anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Context, not a problem.
    Info,
    /// A note worth reading; no action required.
    Low,
    /// Worth a look before accepting the app.
    Medium,
    /// A person must confirm this is intended before the app is accepted.
    High,
    /// The bundle cannot be accepted as it stands.
    Blocker,
}

impl Severity {
    /// The severity as its lowercase word, for the markdown page.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Blocker => "blocker",
        }
    }
}

/// One problem the review found: the stable rule id, its severity, where it is, a message a
/// reviewer can act on, and the security risk it maps to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The stable rule id: an `app_review.*` rule, or a `bundle.*` code passed through.
    pub rule: String,
    /// How serious the finding is.
    pub severity: Severity,
    /// The path or capability the finding concerns, when it is about one. Serialised as
    /// `where`, the report's field name.
    #[serde(rename = "where")]
    pub where_: Option<String>,
    /// What the review observed, phrased for a reviewer.
    pub message: String,
    /// The threat-model risk the rule maps to, such as `R-023`.
    pub risk_id: String,
}

impl Finding {
    /// A finding, with its rule, severity, path, message and risk id.
    pub fn new(
        rule: impl Into<String>,
        severity: Severity,
        where_: Option<String>,
        message: impl Into<String>,
        risk_id: impl Into<String>,
    ) -> Self {
        Self {
            rule: rule.into(),
            severity,
            where_,
            message: message.into(),
            risk_id: risk_id.into(),
        }
    }
}

/// How many findings of each severity a report holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    /// `Info` findings.
    pub info: u32,
    /// `Low` findings.
    pub low: u32,
    /// `Medium` findings.
    pub medium: u32,
    /// `High` findings.
    pub high: u32,
    /// `Blocker` findings.
    pub blocker: u32,
}

impl SeverityCounts {
    /// Counts `severity` once.
    pub(crate) fn add(&mut self, severity: Severity) {
        let slot = match severity {
            Severity::Info => &mut self.info,
            Severity::Low => &mut self.low,
            Severity::Medium => &mut self.medium,
            Severity::High => &mut self.high,
            Severity::Blocker => &mut self.blocker,
        };
        *slot = slot.saturating_add(1);
    }

    /// The total number of findings counted.
    pub fn total(&self) -> u32 {
        self.info
            .saturating_add(self.low)
            .saturating_add(self.medium)
            .saturating_add(self.high)
            .saturating_add(self.blocker)
    }
}
