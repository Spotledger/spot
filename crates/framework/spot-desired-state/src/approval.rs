//! Approval of a bulk plan.
//!
//! A [`BulkPlan`] may be applied only with an [`ApprovedPlan`] issued here. The proof's fields
//! are private and its only constructor is [`ApprovedPlan::authorise`], so no caller can forge
//! one; it names the exact plan digest that was approved. The freshness, requester-vs-approver
//! and distinct-approver checks run inside that constructor ([`ApprovalRules`]), so every
//! [`Approval`] adapter passes through them. A plan changed after approval has a
//! different digest, and the run refuses the stale proof with
//! [`Error::PlanChanged`](crate::Error::PlanChanged).
//!
//! [`Approval`] is the seam the control plane implements. In production that adapter wraps
//! `spot-migration-approval`'s policy and approval records (WP-MIG-04) and maps its decision
//! onto [`ApprovalDecision`]; this crate owns the plan type, so it cannot use that crate's
//! `PlanDigest`, which is bound to a migration `ReleasePlan`. [`MemoryApprovals`] is the
//! in-memory implementation for tests and single-process use.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ids::PlanDigest;
use crate::plan::BulkPlan;

/// The approval flow's answer for one plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ApprovalDecision {
    /// The plan may be applied; the named people approved it.
    Approved {
        /// The approvers whose approval counted.
        approvers: Vec<String>,
        /// When the approval flow reached this decision (unix microseconds).
        decided_at: i64,
    },
    /// The plan is waiting for approvals.
    Pending,
    /// At least one approver rejected the plan.
    Rejected {
        /// The rejecting approvers, in name order.
        by: Vec<String>,
    },
}

impl fmt::Display for ApprovalDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approved { approvers, .. } => {
                write!(formatter, "approved by {}", approvers.join(", "))
            }
            Self::Pending => formatter.write_str("the plan is pending approval"),
            Self::Rejected { by } => write!(formatter, "rejected by {}", by.join(", ")),
        }
    }
}

/// The approval decision for a plan.
pub trait Approval {
    /// Returns the current decision for `plan`.
    fn decide(&self, plan: &BulkPlan, now: i64) -> ApprovalDecision;
}

/// The checks the core applies to every approval, whichever adapter produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalRules {
    max_age: i64,
    min_approvers: usize,
}

impl ApprovalRules {
    /// Rules that accept an approval no older than `max_age` microseconds and at least
    /// `min_approvers` distinct approvers other than the requester. A negative age is zero and
    /// fewer than one approver is one: the rules only ever tighten.
    #[must_use]
    pub fn new(max_age: i64, min_approvers: usize) -> Self {
        Self {
            max_age: max_age.max(0),
            min_approvers: min_approvers.max(1),
        }
    }

    /// The oldest an approval may be, in microseconds.
    #[must_use]
    pub fn max_age(&self) -> i64 {
        self.max_age
    }

    /// How many distinct approvers are needed.
    #[must_use]
    pub fn min_approvers(&self) -> usize {
        self.min_approvers
    }
}

/// The comparison form of an approver or requester name: trimmed and lower-cased.
fn normal(name: &str) -> String {
    name.trim().to_lowercase()
}

fn refuse(reason: &str) -> Error {
    Error::NotApproved {
        decision: reason.to_owned(),
    }
}

/// Proof that one exact plan is approved. Construct it only through
/// [`ApprovedPlan::authorise`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedPlan {
    digest: PlanDigest,
    approvers: Vec<String>,
    approved_at: i64,
}

impl ApprovedPlan {
    /// Submits `plan` to `approval` and returns the proof when it is approved, the approval is
    /// fresh and the approvers are independent of `requester`.
    ///
    /// The core enforces, for every adapter: the decision is an approval; it was made no later
    /// than `now` and no longer than `rules.max_age()` before it; no approver is blank; the
    /// distinct approvers (compared trimmed and case-insensitively) number at least
    /// `rules.min_approvers()`; and `requester`, who asked for the change, is not one of them.
    ///
    /// # Errors
    ///
    /// [`Error::NotApproved`] when the plan is pending or rejected, or the approval is stale,
    /// future-dated, short of approvers, names a blank approver or is signed off by its own
    /// requester, naming the reason.
    pub fn authorise(
        approval: &dyn Approval,
        plan: &BulkPlan,
        requester: &str,
        rules: &ApprovalRules,
        now: i64,
    ) -> Result<Self, Error> {
        let (approvers, decided_at) = match approval.decide(plan, now) {
            ApprovalDecision::Approved {
                approvers,
                decided_at,
            } => (approvers, decided_at),
            decision => return Err(refuse(&decision.to_string())),
        };
        if approvers.iter().any(|name| normal(name).is_empty()) {
            return Err(refuse("the approval names a blank approver"));
        }
        let mut distinct: BTreeMap<String, String> = BTreeMap::new();
        for name in approvers {
            distinct.entry(normal(&name)).or_insert(name);
        }
        if distinct.contains_key(&normal(requester)) {
            return Err(refuse("the requester cannot approve their own plan"));
        }
        if distinct.len() < rules.min_approvers {
            return Err(refuse("the plan has too few distinct approvers"));
        }
        let proof = Self {
            digest: plan.digest(),
            approvers: distinct.into_values().collect(),
            approved_at: decided_at,
        };
        proof.ensure_fresh(rules, now)?;
        Ok(proof)
    }

    /// Checks the approval is not future-dated and not older than the rules allow at `now`.
    ///
    /// # Errors
    ///
    /// [`Error::NotApproved`] when it is stale or dated after `now`.
    pub fn ensure_fresh(&self, rules: &ApprovalRules, now: i64) -> Result<(), Error> {
        if self.approved_at > now {
            return Err(refuse("the approval is dated in the future"));
        }
        if now.saturating_sub(self.approved_at) > rules.max_age {
            return Err(refuse("the approval is too old"));
        }
        Ok(())
    }

    /// The digest of the one plan this proof authorises.
    #[must_use]
    pub fn digest(&self) -> &PlanDigest {
        &self.digest
    }

    /// The distinct approvers whose approval counted.
    #[must_use]
    pub fn approvers(&self) -> &[String] {
        &self.approvers
    }

    /// When the approval flow decided, in unix microseconds.
    #[must_use]
    pub fn approved_at(&self) -> i64 {
        self.approved_at
    }
}

/// An in-memory approval flow, keyed by plan digest.
///
/// A plan with no recorded decision is [`ApprovalDecision::Pending`].
#[derive(Clone, Debug, Default)]
pub struct MemoryApprovals {
    decisions: BTreeMap<PlanDigest, ApprovalDecision>,
}

impl MemoryApprovals {
    /// An approval flow where every plan is pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the decision for exactly `plan`'s current digest.
    pub fn record(&mut self, plan: &BulkPlan, decision: ApprovalDecision) {
        self.decisions.insert(plan.digest(), decision);
    }

    /// Records an approval by `approvers`, decided at `decided_at`, for exactly `plan`'s
    /// current digest.
    pub fn approve(
        &mut self,
        plan: &BulkPlan,
        approvers: impl IntoIterator<Item = String>,
        decided_at: i64,
    ) {
        self.record(
            plan,
            ApprovalDecision::Approved {
                approvers: approvers.into_iter().collect(),
                decided_at,
            },
        );
    }

    /// Records a rejection by `by` for exactly `plan`'s current digest.
    pub fn reject(&mut self, plan: &BulkPlan, by: impl IntoIterator<Item = String>) {
        self.record(
            plan,
            ApprovalDecision::Rejected {
                by: by.into_iter().collect(),
            },
        );
    }
}

impl Approval for MemoryApprovals {
    fn decide(&self, plan: &BulkPlan, _now: i64) -> ApprovalDecision {
        self.decisions
            .get(&plan.digest())
            .cloned()
            .unwrap_or(ApprovalDecision::Pending)
    }
}
