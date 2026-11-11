//! The invariant runner: run every set against the clone after the plan is applied.
//!
//! WP-TST-07 (the L7 invariant suite) owns the invariants themselves; this crate owns the seam.
//! [`InvariantSet`] is a set of invariants — the framework's, an app's or the ERP's — and the
//! runner runs every registered set, never stopping at the first failure, and collects the
//! violations into an [`InvariantReport`]. Because the seam is defined here, the order in which
//! this crate and WP-TST-07 merge does not matter: an adapter wraps a
//! `spot_l7_invariants::InvariantRegistry` in an `InvariantSet` (see the `l7_bridge` test).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::clone::CloneTarget;
use crate::fingerprint::Fingerprint;
use spot_migration_plan::ReleasePlan;

/// One broken promise found against the clone. It names records by entity and id and carries a
/// stable, machine-readable `detail`; it never carries document content or a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantViolation {
    /// The set that found it.
    pub set: String,
    /// The invariant that found it, within the set.
    pub invariant: String,
    /// The entity involved (or `-`).
    pub entity: String,
    /// The record involved (or `-`).
    pub id: String,
    /// What is wrong, as `code key=value ...`.
    pub detail: String,
}

impl InvariantViolation {
    /// A violation.
    pub fn new(
        set: impl Into<String>,
        invariant: impl Into<String>,
        entity: impl Into<String>,
        id: impl Into<String>,
        detail: impl Into<String>,
    ) -> InvariantViolation {
        InvariantViolation {
            set: set.into(),
            invariant: invariant.into(),
            entity: entity.into(),
            id: id.into(),
            detail: detail.into(),
        }
    }
}

/// What a set may look at: the clone, the plan being rehearsed and the post fingerprint.
///
/// The set carries whatever else it needs (its backend, its history and its oracle) in its own
/// fields; the context is the part every set is given.
pub struct InvariantContext<'a> {
    /// The clone the plan was applied to.
    pub clone: &'a CloneTarget,
    /// The plan being rehearsed.
    pub plan: &'a ReleasePlan,
    /// The fingerprint measured after the plan was applied.
    pub fingerprint: &'a Fingerprint,
}

/// A set of invariants to check after the plan is applied.
pub trait InvariantSet: Send + Sync {
    /// The set's stable name, unique in a rehearsal.
    fn name(&self) -> &str;

    /// Checks every invariant in the set and returns all violations found; an empty vector means
    /// the set held. A set never stops at the first violation.
    fn check<'a>(&'a self, ctx: &'a InvariantContext<'a>)
    -> BoxFuture<'a, Vec<InvariantViolation>>;
}

/// The outcome of one set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantSetResult {
    /// The set's name.
    pub set: String,
    /// Whether every invariant in the set held.
    pub passed: bool,
    /// Every violation the set found.
    pub violations: Vec<InvariantViolation>,
}

/// What the invariant step found, in JSON or text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantReport {
    /// One result per set, in registration order.
    pub sets: Vec<InvariantSetResult>,
}

impl InvariantReport {
    /// Runs every set against `ctx`, one after another, never stopping at a failure.
    pub async fn run(
        sets: &[Arc<dyn InvariantSet>],
        ctx: &InvariantContext<'_>,
    ) -> InvariantReport {
        let mut results = Vec::with_capacity(sets.len());
        for set in sets {
            let violations = set.check(ctx).await;
            results.push(InvariantSetResult {
                set: set.name().to_owned(),
                passed: violations.is_empty(),
                violations,
            });
        }
        InvariantReport { sets: results }
    }

    /// Whether every set held.
    pub fn passed(&self) -> bool {
        self.sets.iter().all(|s| s.passed)
    }

    /// The names of the sets that failed.
    pub fn failed_sets(&self) -> Vec<&str> {
        self.sets
            .iter()
            .filter(|s| !s.passed)
            .map(|s| s.set.as_str())
            .collect()
    }

    /// Every violation across every set.
    pub fn violations(&self) -> impl Iterator<Item = &InvariantViolation> {
        self.sets.iter().flat_map(|s| s.violations.iter())
    }

    /// The total number of violations.
    pub fn total(&self) -> u64 {
        self.sets.iter().map(|s| s.violations.len() as u64).sum()
    }
}

/// An invariant set with fixed violations, for tests and for a host that has none.
#[derive(Clone, Debug)]
pub struct StaticInvariantSet {
    name: String,
    violations: Vec<InvariantViolation>,
}

impl StaticInvariantSet {
    /// A set that always reports `violations`.
    pub fn new(name: impl Into<String>, violations: Vec<InvariantViolation>) -> StaticInvariantSet {
        StaticInvariantSet {
            name: name.into(),
            violations,
        }
    }

    /// A set that always holds.
    pub fn passing(name: impl Into<String>) -> StaticInvariantSet {
        StaticInvariantSet::new(name, Vec::new())
    }
}

impl InvariantSet for StaticInvariantSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn check<'a>(
        &'a self,
        _ctx: &'a InvariantContext<'a>,
    ) -> BoxFuture<'a, Vec<InvariantViolation>> {
        let violations = self.violations.clone();
        Box::pin(async move { violations })
    }
}
