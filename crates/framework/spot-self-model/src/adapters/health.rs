//! Runtime health source: fleet-health findings and telemetry signals.
//!
//! The adapter accepts the fleet-health engine's normalised findings directly
//! ([`FleetFinding::from_finding`]) and a neutral signal record for telemetry that names a
//! concrete platform node. Neither carries a document value: a signal's status, rule and
//! sentence are diagnostics, never data.

use spot_fleet_health::{Finding, Health};

use crate::SelfModelError;
use crate::adapters::{push_edge, push_node};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData};

/// One runtime health observation that names the platform node it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthSignalRecord {
    /// The signal's name.
    pub name: String,
    /// Its status (`healthy`, `degraded`, `failing`).
    pub status: String,
    /// The node it observed.
    pub target: NodeRef,
    /// The setting or change that caused it, when known.
    pub cause: Option<NodeRef>,
    /// Unix microseconds.
    pub at: i64,
    /// A non-sensitive one-sentence detail.
    pub detail: String,
    /// The telemetry record that proves it.
    pub evidence: EvidenceRef,
}

/// One fleet-health finding, in the self-model's terms.
///
/// The fleet engine's subjects are tenants and nodes, which are not self-model node kinds.
/// A finding therefore contributes a [`NodeKind::HealthSignal`] node carrying the subject,
/// rule, colour and sentence; when the host knows a platform cause (a setting, a migration)
/// it names it with [`FleetFinding::with_cause`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetFinding {
    /// The signal's name, `subject:rule`.
    pub name: String,
    /// The three-colour health value.
    pub health: Health,
    /// The signal that breached, when the finding is not the freshness register.
    pub signal: Option<String>,
    /// The latest breaching value.
    pub value: Option<i64>,
    /// When the breach began, in unix microseconds.
    pub since: i64,
    /// When the latest reading was taken, in unix microseconds.
    pub observed_at: i64,
    /// The engine's plain-language sentence.
    pub sentence: String,
    /// What a person should do about it.
    pub action: String,
    /// The platform cause, when the host knows one.
    pub cause: Option<NodeRef>,
    /// The fleet-health record that proves it.
    pub evidence: EvidenceRef,
}

impl FleetFinding {
    /// Converts one engine finding. Fleet timestamps are whole seconds; the self-model uses
    /// unix microseconds, so they are scaled here.
    #[must_use]
    pub fn from_finding(finding: &Finding) -> Self {
        let name = format!("{}:{}", finding.subject.display(), finding.rule_id.as_str());
        let observed_at = seconds_to_micros(finding.observed_at.as_secs());
        Self {
            evidence: EvidenceRef::at(
                EvidenceSource::FleetHealth,
                format!("finding:{name}:{}", finding.observed_at.as_secs()),
                observed_at,
            ),
            name,
            health: finding.health,
            signal: finding.signal.map(|signal| signal.as_str().to_owned()),
            value: finding.value,
            since: seconds_to_micros(finding.since.as_secs()),
            observed_at,
            sentence: finding.sentence.clone(),
            action: finding.action.clone(),
            cause: None,
        }
    }

    /// Names the platform cause the host knows about.
    #[must_use]
    pub fn with_cause(mut self, cause: NodeRef) -> Self {
        self.cause = Some(cause);
        self
    }
}

/// Builds health-signal nodes and their `observed_on` / `changed_by` edges.
#[derive(Clone, Debug, Default)]
pub struct HealthAdapter {
    signals: Vec<HealthSignalRecord>,
    findings: Vec<FleetFinding>,
}

impl HealthAdapter {
    /// An adapter with no signals yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one health observation that names a platform node.
    #[must_use]
    pub fn with_signal(mut self, signal: HealthSignalRecord) -> Self {
        self.signals.push(signal);
        self
    }

    /// Adds one fleet-health finding.
    #[must_use]
    pub fn with_finding(mut self, finding: FleetFinding) -> Self {
        self.findings.push(finding);
        self
    }
}

impl SourceAdapter for HealthAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Health
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.signals.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(self.findings.len()).unwrap_or(u64::MAX))
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for signal in &self.signals {
            if !signal.evidence.is_complete() {
                return Err(SelfModelError::EvidenceMissing {
                    what: format!("health:{}", signal.name),
                });
            }
            let signal_ref = NodeRef::new(NodeKind::HealthSignal, signal.name.clone());
            push_node(
                &mut data.nodes,
                Node::new(
                    NodeKind::HealthSignal,
                    signal.name.clone(),
                    vec![signal.evidence.clone()],
                )?
                .with_attribute("status", signal.status.clone())
                .with_attribute("detail", signal.detail.clone())
                .with_attribute("at", signal.at.to_string()),
            );
            push_node(
                &mut data.nodes,
                Node::new(
                    signal.target.kind,
                    signal.target.name.clone(),
                    vec![signal.evidence.clone()],
                )?,
            );
            push_edge(
                &mut data.edges,
                Edge::new(
                    signal_ref.id(),
                    signal.target.id(),
                    EdgeKind::ObservedOn,
                    Some(signal.at),
                    vec![signal.evidence.clone()],
                )?,
            );
            if let Some(cause) = &signal.cause {
                push_node(
                    &mut data.nodes,
                    Node::new(
                        cause.kind,
                        cause.name.clone(),
                        vec![signal.evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        signal_ref.id(),
                        cause.id(),
                        EdgeKind::ChangedBy,
                        Some(signal.at),
                        vec![signal.evidence.clone()],
                    )?,
                );
            }
            let change = if signal.status == "healthy" {
                ChangeKind::Recovered
            } else {
                ChangeKind::Failed
            };
            data.history.push(HistoryEntry::new(
                signal_ref.id(),
                signal.at,
                change,
                signal.detail.clone(),
                signal.evidence.clone(),
            ));
        }

        for finding in &self.findings {
            if !finding.evidence.is_complete() {
                return Err(SelfModelError::EvidenceMissing {
                    what: format!("finding:{}", finding.name),
                });
            }
            let signal_ref = NodeRef::new(NodeKind::HealthSignal, finding.name.clone());
            let mut node = Node::new(
                NodeKind::HealthSignal,
                finding.name.clone(),
                vec![finding.evidence.clone()],
            )?
            .with_attribute("status", finding.health.as_str())
            .with_attribute("health", finding.health.as_str())
            .with_attribute("since", finding.since.to_string())
            .with_attribute("observed_at", finding.observed_at.to_string())
            .with_attribute("detail", finding.sentence.clone())
            .with_attribute("action", finding.action.clone());
            if let Some(signal) = &finding.signal {
                node = node.with_attribute("signal", signal.clone());
            }
            if let Some(value) = finding.value {
                node = node.with_attribute("value", value.to_string());
            }
            push_node(&mut data.nodes, node);
            if let Some(cause) = &finding.cause {
                push_node(
                    &mut data.nodes,
                    Node::new(
                        cause.kind,
                        cause.name.clone(),
                        vec![finding.evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        signal_ref.id(),
                        cause.id(),
                        EdgeKind::ChangedBy,
                        Some(finding.observed_at),
                        vec![finding.evidence.clone()],
                    )?,
                );
            }
            let change = match finding.health {
                Health::Green => ChangeKind::Recovered,
                Health::Amber => ChangeKind::Observed,
                Health::Red => ChangeKind::Failed,
            };
            data.history.push(HistoryEntry::new(
                signal_ref.id(),
                finding.observed_at,
                change,
                finding.sentence.clone(),
                finding.evidence.clone(),
            ));
        }

        Ok(data)
    }
}

fn seconds_to_micros(seconds: i64) -> i64 {
    seconds.saturating_mul(1_000_000)
}
