//! The partition map: which node owns each scope key, effective-dated and audited.
//!
//! [`PartitionMap`] is an immutable snapshot of the ownership assignments **effective at one
//! instant**. [`PartitionRegistry`] is the store that produces snapshots and records changes;
//! [`MemoryPartitionRegistry`] is the in-memory implementation for one node and for tests. The
//! database-backed registry (the hub's control plane, ADR-063 section 4) is a later task behind
//! the same trait.
//!
//! # One writer per key
//!
//! An assignment gives one scope key to one node over an effective interval. The registry
//! refuses an assignment that would give a key to a second node while the first still owns it
//! ([`PartitionError::OwnershipConflict`]), so the map never has two owners for one key at one
//! instant. That is the mechanism behind "two-writer attempts are rejected by design".
//!
//! # Effective-dated and audited
//!
//! Every change carries an `effective_from` and an optional `effective_to`; instants are
//! arguments, never read from a clock. [`PartitionRegistry::history`] returns the append-only
//! audit trail: who changed what, why, and when it takes effect.

use std::sync::{Mutex, MutexGuard};

use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::error::PartitionError;
use crate::scope::ScopeKey;

/// One ownership assignment: a scope key, its owning node, its version and its effective
/// interval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// The scope key.
    pub scope: ScopeKey,
    /// The node that owns it over the interval.
    pub node: NodeId,
    /// The assignment's version for this scope key, from 1 and increasing.
    pub version: u64,
    /// The instant the assignment takes effect.
    pub effective_from: OffsetDateTime,
    /// The instant it stops taking effect, or `None` for open-ended.
    pub effective_to: Option<OffsetDateTime>,
}

/// A request to assign a scope key to a node (the registry assigns the version).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentRequest {
    /// The scope key to assign.
    pub scope: ScopeKey,
    /// The node to assign it to.
    pub node: NodeId,
    /// The instant the assignment takes effect.
    pub effective_from: OffsetDateTime,
    /// The instant it stops taking effect, or `None` for open-ended.
    pub effective_to: Option<OffsetDateTime>,
    /// Who made the change, for the audit trail.
    pub actor: String,
    /// Why, for the audit trail.
    pub reason: String,
    /// The instant the change was recorded.
    pub at: OffsetDateTime,
}

/// One audited ownership change, as [`PartitionRegistry::history`] returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionChange {
    /// The scope key that changed hands.
    pub scope: ScopeKey,
    /// The node it now belongs to.
    pub node: NodeId,
    /// The version this change produced.
    pub version: u64,
    /// The instant it takes effect.
    pub effective_from: OffsetDateTime,
    /// The instant it stops taking effect, or `None`.
    pub effective_to: Option<OffsetDateTime>,
    /// Who made the change.
    pub actor: String,
    /// Why.
    pub reason: String,
    /// The instant the change was recorded.
    pub at: OffsetDateTime,
}

/// An immutable ownership snapshot: which node owns each scope key at one instant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionMap {
    assignments: Vec<Assignment>,
}

impl PartitionMap {
    /// A map over the assignments already resolved to one instant.
    #[must_use]
    pub fn new(assignments: Vec<Assignment>) -> Self {
        Self { assignments }
    }

    /// The assignment for `scope`, the highest version if several apply.
    #[must_use]
    pub fn assignment(&self, scope: &ScopeKey) -> Option<&Assignment> {
        self.assignments
            .iter()
            .filter(|a| &a.scope == scope)
            .max_by_key(|a| a.version)
    }

    /// The node that owns `scope`, or `None` when no assignment covers it.
    #[must_use]
    pub fn owner(&self, scope: &ScopeKey) -> Option<&NodeId> {
        self.assignment(scope).map(|a| &a.node)
    }

    /// The version of the assignment that owns `scope`, if any.
    #[must_use]
    pub fn version(&self, scope: &ScopeKey) -> Option<u64> {
        self.assignment(scope).map(|a| a.version)
    }

    /// Every assignment in the snapshot.
    #[must_use]
    pub fn entries(&self) -> &[Assignment] {
        &self.assignments
    }
}

/// The store of ownership assignments.
///
/// A host reads ownership through [`PartitionRegistry::map_at`] and changes it through
/// [`PartitionRegistry::assign`]. All methods take `&self`; an implementation keeps its own
/// interior synchronisation.
pub trait PartitionRegistry: Send + Sync {
    /// Records an assignment, returning the version it produced.
    ///
    /// # Errors
    ///
    /// [`PartitionError::InvalidArgument`] when `effective_to` is not after `effective_from`,
    /// and [`PartitionError::OwnershipConflict`] when the interval overlaps an assignment of
    /// the same key to another node.
    fn assign(&self, request: AssignmentRequest) -> Result<u64, PartitionError>;

    /// The ownership map effective at `at`.
    fn map_at(&self, at: OffsetDateTime) -> PartitionMap;

    /// The append-only audit trail, oldest first.
    fn history(&self) -> Vec<PartitionChange>;
}

/// An in-memory [`PartitionRegistry`] for one node and for tests.
#[derive(Debug, Default)]
pub struct MemoryPartitionRegistry {
    inner: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    assignments: Vec<Assignment>,
    history: Vec<PartitionChange>,
}

impl MemoryPartitionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether `[a_from, a_to)` and `[b_from, b_to)` overlap, with `None` meaning open-ended.
fn overlaps(
    a_from: OffsetDateTime,
    a_to: Option<OffsetDateTime>,
    b_from: OffsetDateTime,
    b_to: Option<OffsetDateTime>,
) -> bool {
    let a_before_b_ends = b_to.is_none_or(|b_to| a_from < b_to);
    let b_before_a_ends = a_to.is_none_or(|a_to| b_from < a_to);
    a_before_b_ends && b_before_a_ends
}

impl PartitionRegistry for MemoryPartitionRegistry {
    fn assign(&self, request: AssignmentRequest) -> Result<u64, PartitionError> {
        if let Some(to) = request.effective_to
            && to <= request.effective_from
        {
            return Err(PartitionError::InvalidArgument {
                reason: "effective_to is not after effective_from".to_owned(),
            });
        }
        let mut state = lock(&self.inner);
        let version = state
            .assignments
            .iter()
            .filter(|a| a.scope == request.scope)
            .map(|a| a.version)
            .max()
            .unwrap_or(0)
            + 1;
        for existing in &state.assignments {
            if existing.scope == request.scope
                && existing.node != request.node
                && overlaps(
                    request.effective_from,
                    request.effective_to,
                    existing.effective_from,
                    existing.effective_to,
                )
            {
                return Err(PartitionError::OwnershipConflict {
                    scope: request.scope.to_string(),
                    owner: existing.node.to_string(),
                    challenger: request.node.to_string(),
                });
            }
        }
        state.assignments.push(Assignment {
            scope: request.scope.clone(),
            node: request.node.clone(),
            version,
            effective_from: request.effective_from,
            effective_to: request.effective_to,
        });
        state.history.push(PartitionChange {
            scope: request.scope,
            node: request.node,
            version,
            effective_from: request.effective_from,
            effective_to: request.effective_to,
            actor: request.actor,
            reason: request.reason,
            at: request.at,
        });
        Ok(version)
    }

    fn map_at(&self, at: OffsetDateTime) -> PartitionMap {
        let state = lock(&self.inner);
        let assignments = state
            .assignments
            .iter()
            .filter(|a| a.effective_from <= at && a.effective_to.is_none_or(|to| at < to))
            .cloned()
            .collect();
        PartitionMap::new(assignments)
    }

    fn history(&self) -> Vec<PartitionChange> {
        lock(&self.inner).history.clone()
    }
}
