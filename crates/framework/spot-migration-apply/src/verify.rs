//! The verify gate: the only thing that admits users.
//!
//! After every step the engine runs [`VerifyPort::verify`]. The reference [`PlanVerifier`] runs
//! the plan's own re-diff ([`verify_applied`](spot_migration_plan::verify_applied): the target
//! against what actually happened) and every registered [`InvariantSet`]. Only a pass admits
//! users; a fail triggers the snapshot restore and discards the held effects.

use std::sync::Arc;

use spot_migration_plan::{ReleasePlan, SystemDescription};

use crate::BoxFuture;
use crate::error::Result;

/// Provides the system as it actually is after the plan was applied, so the plan's re-diff can
/// compare it with the plan's target.
pub trait AppliedState: Send + Sync {
    /// The system after apply.
    ///
    /// # Errors
    /// A backend error when the description cannot be read.
    fn applied(&self) -> BoxFuture<'_, Result<SystemDescription>>;
}

/// One broken promise found by an invariant set.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Violation {
    /// The invariant that found it, within the set.
    pub invariant: String,
    /// What is wrong, as a secret-free detail.
    pub detail: String,
}

impl Violation {
    /// A violation.
    pub fn new(invariant: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            invariant: invariant.into(),
            detail: detail.into(),
        }
    }
}

/// A set of invariants checked after apply.
///
/// WP-TST-07 (the L7 invariant suite) owns the invariants themselves; this crate owns the seam.
pub trait InvariantSet: Send + Sync {
    /// The set's stable name, unique in a run.
    fn name(&self) -> &str;

    /// Checks every invariant in the set and returns all violations found; an empty vector means
    /// the set held. A set never stops at the first violation.
    fn check<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, Vec<Violation>>;
}

/// The outcome of one invariant set.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InvariantResult {
    /// The set's name.
    pub set: String,
    /// Whether every invariant in the set held.
    pub passed: bool,
    /// Every violation the set found.
    pub violations: Vec<Violation>,
}

/// What verification found.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerifyOutcome {
    /// Whether verification passed: no residual difference and no violation.
    pub passed: bool,
    /// The ids of the residual differences the plan re-diff found.
    pub mismatches: Vec<String>,
    /// One result per invariant set, in registration order.
    pub invariants: Vec<InvariantResult>,
}

impl VerifyOutcome {
    /// The total number of invariant violations.
    #[must_use]
    pub fn violations(&self) -> u64 {
        self.invariants
            .iter()
            .map(|result| u64::try_from(result.violations.len()).unwrap_or(u64::MAX))
            .sum()
    }
}

/// Runs verification for a plan.
pub trait VerifyPort: Send + Sync {
    /// Verifies the plan was applied.
    ///
    /// # Errors
    /// A backend error when verification cannot be run.
    fn verify<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, Result<VerifyOutcome>>;
}

/// The reference verifier: the plan's re-diff plus every registered invariant set.
pub struct PlanVerifier {
    applied: Arc<dyn AppliedState>,
    invariants: Vec<Arc<dyn InvariantSet>>,
}

impl PlanVerifier {
    /// A verifier over `applied` with no invariant sets.
    #[must_use]
    pub fn new(applied: Arc<dyn AppliedState>) -> Self {
        Self {
            applied,
            invariants: Vec::new(),
        }
    }

    /// The same verifier with the given invariant sets.
    #[must_use]
    pub fn with_invariants(mut self, invariants: Vec<Arc<dyn InvariantSet>>) -> Self {
        self.invariants = invariants;
        self
    }
}

impl VerifyPort for PlanVerifier {
    fn verify<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, Result<VerifyOutcome>> {
        Box::pin(async move {
            let after = self.applied.applied().await?;
            let mismatches = spot_migration_plan::verify_applied(plan, &after);
            let mismatch_ids: Vec<String> = mismatches
                .mismatches
                .iter()
                .map(|change| change.id.clone())
                .collect();
            let mut invariants = Vec::with_capacity(self.invariants.len());
            for set in &self.invariants {
                let violations = set.check(plan).await;
                invariants.push(InvariantResult {
                    set: set.name().to_owned(),
                    passed: violations.is_empty(),
                    violations,
                });
            }
            let passed = mismatch_ids.is_empty() && invariants.iter().all(|result| result.passed);
            Ok(VerifyOutcome {
                passed,
                mismatches: mismatch_ids,
                invariants,
            })
        })
    }
}

/// An invariant set with fixed violations, for tests and for a host that has none.
#[derive(Clone, Debug)]
pub struct StaticInvariantSet {
    name: String,
    violations: Vec<Violation>,
}

impl StaticInvariantSet {
    /// A set that always reports `violations`.
    pub fn new(name: impl Into<String>, violations: Vec<Violation>) -> Self {
        Self {
            name: name.into(),
            violations,
        }
    }

    /// A set that always holds.
    pub fn passing(name: impl Into<String>) -> Self {
        Self::new(name, Vec::new())
    }
}

impl InvariantSet for StaticInvariantSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn check<'a>(&'a self, _plan: &'a ReleasePlan) -> BoxFuture<'a, Vec<Violation>> {
        let violations = self.violations.clone();
        Box::pin(async move { violations })
    }
}
