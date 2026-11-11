//! The seam the apply engine plugs into: [`NodeMigrator`] runs one tenant's migration, and
//! [`run_nodes`] dispatches a wave in bounded batches.
//!
//! The trait is deliberately synchronous and takes everything it needs as arguments: the node,
//! the release and the epoch. The apply engine (WP-MIG-07) adapts its own machinery behind it;
//! this crate decides **which** nodes and **how many at once**, never how a migration runs.
//!
//! [`run_nodes`] is the reference executor. It calls the migrator once per node in order and
//! groups the calls into batches of at most [`ConcurrencyLimit`] nodes. A real engine runs one
//! batch concurrently and calls the next only when the batch finished; the batch size is
//! therefore the concurrency limit the rollout plan declares. Keeping this loop sequential makes
//! the simulator deterministic while still enforcing the same node-level limit.

use crate::epoch::Epoch;
use crate::error::MigrateError;
use crate::ids::{NodeId, ReleaseId};
use crate::plan::ConcurrencyLimit;

/// One node's migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrateRequest {
    /// The node to migrate.
    pub node: NodeId,
    /// The release being applied.
    pub release: ReleaseId,
    /// The migration epoch the network assigned to this release.
    pub epoch: Epoch,
}

/// What a node's migration produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrateOutcome {
    /// The epoch the node finished verify at.
    pub epoch: Epoch,
    /// Whether verify passed.
    pub verified: bool,
    /// An operator-facing note, safe to log.
    pub detail: Option<String>,
}

impl MigrateOutcome {
    /// A verified outcome at `epoch`.
    #[must_use]
    pub fn verified(epoch: Epoch) -> Self {
        Self {
            epoch,
            verified: true,
            detail: None,
        }
    }

    /// A finished but unverified outcome at `epoch`.
    #[must_use]
    pub fn unverified(epoch: Epoch, detail: impl Into<String>) -> Self {
        Self {
            epoch,
            verified: false,
            detail: Some(detail.into()),
        }
    }
}

/// The apply engine's adapter for one node's migration.
pub trait NodeMigrator {
    /// Migrates one node and reports the epoch it verified at.
    ///
    /// # Errors
    ///
    /// [`MigrateError`] when the migration could not run to completion.
    fn migrate(&mut self, request: &MigrateRequest) -> Result<MigrateOutcome, MigrateError>;
}

/// One node's result from [`run_nodes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMigration {
    /// The node.
    pub node: NodeId,
    /// The migrator's outcome.
    pub outcome: Result<MigrateOutcome, MigrateError>,
}

/// Migrates `nodes` in batches of at most `limit`.
///
/// A failing node does not stop the rest; the caller turns each [`NodeMigration`] into an
/// observation and a failure event, and the state machine decides whether the fleet pauses.
#[must_use]
pub fn run_nodes<M: NodeMigrator + ?Sized>(
    migrator: &mut M,
    release: &ReleaseId,
    epoch: Epoch,
    nodes: &[NodeId],
    limit: ConcurrencyLimit,
) -> Vec<NodeMigration> {
    let mut results = Vec::with_capacity(nodes.len());
    for batch in nodes.chunks(limit.get()) {
        for node in batch {
            let request = MigrateRequest {
                node: node.clone(),
                release: release.clone(),
                epoch,
            };
            results.push(NodeMigration {
                node: node.clone(),
                outcome: migrator.migrate(&request),
            });
        }
    }
    results
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Fixed {
        failing: Option<NodeId>,
        calls: usize,
    }

    impl NodeMigrator for Fixed {
        fn migrate(&mut self, request: &MigrateRequest) -> Result<MigrateOutcome, MigrateError> {
            self.calls += 1;
            if self.failing.as_ref() == Some(&request.node) {
                Err(MigrateError::new("seeded failure"))
            } else {
                Ok(MigrateOutcome::verified(request.epoch))
            }
        }
    }

    #[test]
    fn every_node_runs_even_after_a_failure() {
        let nodes: Vec<NodeId> = ["a", "b", "c"]
            .iter()
            .map(|name| NodeId::new(*name).expect("a valid test node"))
            .collect();
        let failing = nodes[1].clone();
        let mut migrator = Fixed {
            failing: Some(failing.clone()),
            calls: 0,
        };
        let results = run_nodes(
            &mut migrator,
            &ReleaseId::new("r1").expect("a valid release"),
            Epoch::new(2),
            &nodes,
            ConcurrencyLimit::new(2).expect("a positive limit"),
        );
        assert_eq!(migrator.calls, 3);
        assert!(results[0].outcome.is_ok());
        assert!(results[1].outcome.is_err());
        assert!(results[2].outcome.is_ok());
        assert_eq!(results[1].node, failing);
    }
}
