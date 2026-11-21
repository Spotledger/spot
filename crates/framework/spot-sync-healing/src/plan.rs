//! From a comparison to the repairs the driver may run, with the direction rule applied.

use spot_sync_reconcile::{Finding, NodeRole, ReconcileReport, Repair, plan_repairs};

use crate::{ActionKind, HealingError, Target};

/// How urgent a repair is. A higher value is done first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// A record the owner does not hold. It is only ever reported by an owner-to-mirror
    /// comparison; the repair overwrites, it cannot delete.
    Orphan,
    /// A record missing or behind at the mirror.
    Record,
    /// A count, total, GL balance or stock quantity differs: money and stock first.
    Aggregate,
}

/// One comparison result handed to the driver.
#[derive(Debug, Clone)]
pub struct Observation {
    /// The destination node: the mirror that would be repaired.
    pub node: String,
    /// The reconciliation result for one scope, computed with the source as `local`.
    pub report: ReconcileReport,
    /// The role of the node the records would come from. `None` means not known, and nothing
    /// is repaired.
    pub source_role: Option<NodeRole>,
    /// The role of the destination node. `None` means not known, and nothing is repaired.
    pub destination_role: Option<NodeRole>,
}

/// One repair the driver may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Planned {
    pub(crate) target: Target,
    pub(crate) version: Option<u64>,
    pub(crate) priority: Priority,
    pub(crate) ladder: &'static [ActionKind],
}

const RECORD_LADDER: &[ActionKind] = &[ActionKind::RepullRecord, ActionKind::RequestSnapshot];
const ORPHAN_LADDER: &[ActionKind] = &[ActionKind::RequestSnapshot];
const SCOPE_LADDER: &[ActionKind] = &[
    ActionKind::RebuildDigest,
    ActionKind::ResendScope,
    ActionKind::RequestSnapshot,
];

/// The repairs for one observation, or the reason there are none.
///
/// Anything uncertain is a refusal: a missing role, a source that is not an owner or a
/// destination that is not a mirror, a mirror newer than its owner, a repair that does not
/// point at the destination.
pub(crate) fn plan(observation: &Observation) -> Result<Vec<Planned>, HealingError> {
    let scope = observation.report.scope.label();
    let ambiguous = || HealingError::DirectionAmbiguous {
        node: observation.node.clone(),
        scope: scope.clone(),
    };
    let (Some(source), Some(destination)) = (observation.source_role, observation.destination_role)
    else {
        return Err(ambiguous());
    };
    if source != NodeRole::Owner || destination != NodeRole::Mirror {
        return Err(ambiguous());
    }
    let mirror_ahead = observation.report.findings.iter().any(|finding| {
        matches!(finding, Finding::VersionMismatch { local, remote, .. } if remote > local)
    });
    if mirror_ahead {
        return Err(HealingError::MirrorAhead {
            node: observation.node.clone(),
            scope,
        });
    }
    let repairs = plan_repairs(&observation.report, source, destination, &observation.node)
        .map_err(|_| ambiguous())?;
    let mut planned = Vec::with_capacity(repairs.len());
    for repair in &repairs {
        let (destination_node, item) = match repair {
            Repair::Resend {
                destination,
                entity,
                name,
                version,
            } => (
                destination,
                Planned {
                    target: Target::Record {
                        entity: entity.as_str().to_owned(),
                        name: name.clone(),
                    },
                    version: Some(version.0),
                    priority: Priority::Record,
                    ladder: RECORD_LADDER,
                },
            ),
            Repair::RebuildMirror {
                destination,
                entity,
                name,
            } => (
                destination,
                Planned {
                    target: Target::Record {
                        entity: entity.as_str().to_owned(),
                        name: name.clone(),
                    },
                    version: None,
                    priority: Priority::Orphan,
                    ladder: ORPHAN_LADDER,
                },
            ),
            Repair::RebuildScope { destination, .. } => (
                destination,
                Planned {
                    target: Target::Scope,
                    version: None,
                    priority: Priority::Aggregate,
                    ladder: SCOPE_LADDER,
                },
            ),
        };
        if destination_node != &observation.node {
            return Err(ambiguous());
        }
        planned.push(item);
    }
    Ok(planned)
}
