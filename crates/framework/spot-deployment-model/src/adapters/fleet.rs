//! The fleet-health adapter: hub reachability and the last successful sync.
//!
//! `spot-fleet-health` normalises the site's telemetry. This adapter reads one node's entry:
//! the node is unreachable when it has no data, when its telemetry is stale, or when its
//! health-probe finding is red. The last successful sync is the newest observation fleet health
//! holds (seconds, converted to the model's microseconds) — the model never reads a clock.

use spot_fleet_health::{FleetHealth, Health, NodeId, Signal, Subject};
use spot_self_model::EvidenceRef;

use crate::DeploymentError;
use crate::facts::Facts;
use crate::model::{Connectivity, FactSourceKind};

/// The fleet-health ground truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetFacts {
    epoch: u64,
    connectivity: Option<Connectivity>,
    evidence: Option<EvidenceRef>,
}

impl FleetFacts {
    /// An empty slice at `epoch`.
    #[must_use]
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            connectivity: None,
            evidence: None,
        }
    }

    /// Reads one node's reachability and freshness from a fleet-health evaluation.
    #[must_use]
    pub fn from_health(epoch: u64, health: &FleetHealth, node: &NodeId) -> Self {
        let entry = health.node(node);
        let hub_reachable = entry.is_some_and(|entry| {
            !entry.stale
                && !entry.no_data
                && !entry.findings.iter().any(|finding| {
                    finding.health >= Health::Red && finding.signal == Some(Signal::HealthProbe)
                })
        });
        let last_sync_at = entry
            .and_then(|entry| entry.last_observed_at)
            .map(|at| at.as_secs().saturating_mul(1_000_000));
        Self {
            epoch,
            connectivity: Some(Connectivity::new(hub_reachable, last_sync_at)),
            evidence: None,
        }
    }

    /// Reads a subject directly, for a caller that has the entry and not the whole view.
    #[must_use]
    pub fn from_subject(epoch: u64, subject: &Subject, health: &FleetHealth) -> Self {
        match subject.node() {
            Some(node) => Self::from_health(epoch, health, node),
            None => Self {
                epoch,
                connectivity: None,
                evidence: None,
            },
        }
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl crate::facts::FactSource for FleetFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::FleetHealth
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(Facts {
            connectivity: self.connectivity.clone(),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
