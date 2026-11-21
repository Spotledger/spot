//! Repair **suggestions** from findings: never repair an owner from a mirror.
//!
//! A reconciliation cycle does not fix anything itself. Applying repairs is the self-healing
//! layer's job (ADR-067), and it requires ownership: a record has one owner, and only the owner
//! is ever the source of truth (ADR-062 G1, ADR-067 rule 6). [`plan_repairs`] therefore refuses
//! to run with a mirror as the source of truth, and every suggestion it makes targets the
//! **mirror**: resend a record from its owner, or rebuild the mirror's copy from its owner.
//! Nothing it returns can edit, delete or overwrite an owner's record.

use crate::error::{ReconcileError, ReconcileResult};
use crate::reconcile::{Finding, ReconcileReport};
use crate::scope::Scope;
use spot_storage_contract::{Name, Version};

/// Which side of a reconciliation a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// The record's owner: the only source of truth.
    Owner,
    /// A node holding a read-only mirror of an owner's records.
    Mirror,
}

/// One suggested repair. Applying it is the self-healing layer's job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// Ask the owner to resend this record version to the mirror.
    Resend {
        /// The node to resend to (always the mirror).
        destination: String,
        /// The record's entity.
        entity: Name,
        /// The record's name.
        name: String,
        /// The version the owner holds and the mirror must receive.
        version: Version,
    },
    /// Rebuild the mirror's copy of one record from its owner.
    RebuildMirror {
        /// The node to rebuild (always the mirror).
        destination: String,
        /// The record's entity.
        entity: Name,
        /// The record's name.
        name: String,
    },
    /// Rebuild the mirror's whole scope from its owner, when only aggregates differ.
    RebuildScope {
        /// The node to rebuild (always the mirror).
        destination: String,
        /// The scope to rebuild.
        scope: Scope,
    },
}

/// Turns findings into repair suggestions, given which side owns the records.
///
/// `local_role` must be [`NodeRole::Owner`] and `remote_role` [`NodeRole::Mirror`], because the
/// findings were computed with the owner as `local` (see [`crate::reconcile`]). Any other
/// combination fails with `sync_reconcile.mirror_as_source`: this crate never repairs an owner
/// from a mirror.
pub fn plan_repairs(
    report: &ReconcileReport,
    local_role: NodeRole,
    remote_role: NodeRole,
    mirror_node: &str,
) -> ReconcileResult<Vec<Repair>> {
    if local_role != NodeRole::Owner || remote_role != NodeRole::Mirror {
        let (entity, name) = first_record(report)
            .unwrap_or_else(|| (report.scope.key_type.clone(), report.scope.value.clone()));
        return Err(ReconcileError::MirrorAsSource { entity, name });
    }

    let mut repairs = Vec::new();
    let mut scope_rebuild = false;
    for finding in &report.findings {
        match finding {
            Finding::MissingAtReceiver {
                entity,
                name,
                version,
            }
            | Finding::VersionMismatch {
                entity,
                name,
                local: version,
                ..
            } => repairs.push(Repair::Resend {
                destination: mirror_node.to_owned(),
                entity: entity.clone(),
                name: name.clone(),
                version: *version,
            }),
            Finding::ExtraAtReceiver { entity, name, .. } => repairs.push(Repair::RebuildMirror {
                destination: mirror_node.to_owned(),
                entity: entity.clone(),
                name: name.clone(),
            }),
            Finding::TotalMismatch { .. } => scope_rebuild = true,
        }
    }
    if scope_rebuild {
        repairs.push(Repair::RebuildScope {
            destination: mirror_node.to_owned(),
            scope: report.scope.clone(),
        });
    }
    Ok(repairs)
}

fn first_record(report: &ReconcileReport) -> Option<(String, String)> {
    report.findings.iter().find_map(|finding| match finding {
        Finding::MissingAtReceiver { entity, name, .. }
        | Finding::ExtraAtReceiver { entity, name, .. }
        | Finding::VersionMismatch { entity, name, .. } => Some((entity.to_string(), name.clone())),
        Finding::TotalMismatch { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::reconcile::Measure;

    fn report(findings: Vec<Finding>) -> ReconcileReport {
        ReconcileReport {
            scope: Scope::new("company", "C1"),
            findings,
            rounds: 1,
            digest_bytes: 64,
        }
    }

    #[test]
    fn missing_and_version_findings_resend_from_the_owner() {
        let report = report(vec![
            Finding::MissingAtReceiver {
                entity: Name::new("item").unwrap(),
                name: "A".into(),
                version: Version(4),
            },
            Finding::VersionMismatch {
                entity: Name::new("item").unwrap(),
                name: "B".into(),
                local: Version(2),
                remote: Version(1),
            },
        ]);
        let repairs = plan_repairs(&report, NodeRole::Owner, NodeRole::Mirror, "branch").unwrap();
        assert_eq!(repairs.len(), 2);
        assert!(repairs.iter().all(|r| match r {
            Repair::Resend { destination, .. } => destination == "branch",
            _ => false,
        }));
    }

    #[test]
    fn an_extra_record_rebuilds_the_mirror_not_the_owner() {
        let report = report(vec![Finding::ExtraAtReceiver {
            entity: Name::new("item").unwrap(),
            name: "Z".into(),
            version: Version(1),
        }]);
        let repairs = plan_repairs(&report, NodeRole::Owner, NodeRole::Mirror, "branch").unwrap();
        assert_eq!(
            repairs,
            vec![Repair::RebuildMirror {
                destination: "branch".into(),
                entity: Name::new("item").unwrap(),
                name: "Z".into(),
            }]
        );
    }

    #[test]
    fn an_aggregate_only_mismatch_rebuilds_the_scope() {
        let report = report(vec![Finding::TotalMismatch {
            measure: Measure::GlBalance {
                account: "1100".into(),
            },
            local: 100,
            remote: 90,
        }]);
        let repairs = plan_repairs(&report, NodeRole::Owner, NodeRole::Mirror, "branch").unwrap();
        assert!(
            matches!(repairs.as_slice(), [Repair::RebuildScope { destination, .. }] if destination == "branch")
        );
    }

    #[test]
    fn a_mirror_is_never_the_source_of_truth() {
        let report = report(vec![Finding::MissingAtReceiver {
            entity: Name::new("item").unwrap(),
            name: "A".into(),
            version: Version(1),
        }]);
        let error = plan_repairs(&report, NodeRole::Mirror, NodeRole::Owner, "hub").unwrap_err();
        assert_eq!(error.code(), "sync_reconcile.mirror_as_source");
    }
}
