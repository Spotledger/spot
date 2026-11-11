//! The only way to an [`ApplyTicket`]: [`authorise_apply`].
//!
//! The ticket's fields are private and its only constructor is private to this module, so the
//! apply engine can require one and know it was issued for exactly one plan digest.
//!
//! ```compile_fail
//! use spot_migration_approval::ApplyTicket;
//! // No public constructor and no public fields: this does not compile.
//! let _forged = ApplyTicket { digest: todo!(), risk: todo!(), approvers: vec![], issued_at: 0 };
//! ```

use std::collections::BTreeMap;

use spot_migration_plan::{ReleasePlan, Risk};

use crate::approval::{Approval, Checks, Decision};
use crate::digest::PlanDigest;
use crate::error::Refusal;
use crate::policy::ApprovalPolicy;

/// Proof that this exact plan is approved under the site's policy. Required by the apply
/// engine (WP-MIG-07).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyTicket {
    digest: PlanDigest,
    risk: Risk,
    approvers: Vec<String>,
    issued_at: i64,
}

impl ApplyTicket {
    fn new(digest: PlanDigest, risk: Risk, approvers: Vec<String>, issued_at: i64) -> Self {
        Self {
            digest,
            risk,
            approvers,
            issued_at,
        }
    }

    /// The digest of the one plan this ticket authorises. Apply recomputes the digest of the
    /// plan in hand and refuses a mismatch.
    #[must_use]
    pub fn digest(&self) -> &PlanDigest {
        &self.digest
    }

    /// The plan's risk class when the ticket was issued.
    #[must_use]
    pub fn risk(&self) -> Risk {
        self.risk
    }

    /// The distinct approvers whose approvals counted (empty for an auto-approved plan).
    #[must_use]
    pub fn approvers(&self) -> &[String] {
        &self.approvers
    }

    /// When the ticket was issued (unix microseconds, the `now` passed in).
    #[must_use]
    pub fn issued_at(&self) -> i64 {
        self.issued_at
    }

    /// Whether this ticket authorises exactly this plan.
    #[must_use]
    pub fn matches(&self, plan: &ReleasePlan) -> bool {
        self.digest == PlanDigest::of(plan)
    }
}

/// The plan's risk, recomputed from its changes so a forged `risk` field cannot lower it.
#[must_use]
pub fn effective_risk(plan: &ReleasePlan) -> Risk {
    plan.changes
        .iter()
        .map(|c| c.risk)
        .fold(plan.risk, std::cmp::Ord::max)
}

/// How a plan stands against the approvals in hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// The plan's digest.
    pub digest: PlanDigest,
    /// The plan's effective risk.
    pub risk: Risk,
    /// Distinct valid approvers needed.
    pub required: u32,
    /// Distinct approvers whose approval currently counts, in name order.
    pub valid: Vec<String>,
    /// Approvers whose latest decision is a rejection.
    pub rejected_by: Vec<String>,
    /// Approvers whose latest approval has expired.
    pub expired: Vec<String>,
    /// First approver lacking a required, verified hardware signature.
    pub missing_signature: Option<String>,
    /// Approvals exist, but only for other digests (the plan changed after approval).
    pub plan_changed: bool,
}

impl Evaluation {
    /// The refusal for this evaluation, or `None` when the plan may be applied.
    #[must_use]
    pub fn refusal(&self) -> Option<Refusal> {
        if let Some(by) = self.rejected_by.first() {
            return Some(Refusal::Rejected { by: by.clone() });
        }
        let valid = u32::try_from(self.valid.len()).unwrap_or(u32::MAX);
        if valid >= self.required {
            return None;
        }
        if self.plan_changed {
            return Some(Refusal::PlanChanged);
        }
        if let Some(approver) = &self.missing_signature {
            return Some(Refusal::SignatureRequired {
                approver: approver.clone(),
            });
        }
        if valid == 0 && !self.expired.is_empty() {
            return Some(Refusal::Expired);
        }
        Some(Refusal::NotEnoughApprovals {
            valid,
            required: self.required,
        })
    }
}

/// Evaluates a plan against `approvals` (any digests; only those for this plan count).
#[must_use]
pub fn evaluate(
    plan: &ReleasePlan,
    approvals: &[Approval],
    policy: &ApprovalPolicy,
    now: i64,
    checks: &Checks<'_>,
) -> Evaluation {
    let digest = PlanDigest::of(plan);
    let risk = effective_risk(plan);
    let need = policy.requirement(risk);

    // The latest decision per approver, for this exact digest. A later timestamp wins; at an
    // equal timestamp the later record wins. Records dated after `now` are ignored.
    let mut latest: BTreeMap<&str, &Approval> = BTreeMap::new();
    for approval in approvals {
        if approval.plan_digest != digest || approval.at > now {
            continue;
        }
        let keep = latest
            .get(approval.approver.as_str())
            .is_none_or(|current| approval.at >= current.at);
        if keep {
            latest.insert(approval.approver.as_str(), approval);
        }
    }

    let mut valid = Vec::new();
    let mut rejected_by = Vec::new();
    let mut expired = Vec::new();
    let mut missing_signature = None;
    for (who, approval) in latest {
        if approval.decision == Decision::Reject {
            rejected_by.push(who.to_owned());
            continue;
        }
        if now >= approval.at.saturating_add(policy.expiry_micros) {
            expired.push(who.to_owned());
            continue;
        }
        if let Some(role) = &need.role
            && &approval.role != role
        {
            continue;
        }
        if policy.forbid_author_approval && checks.authorship.is_author(&digest, who) {
            continue;
        }
        if need.hardware_signature {
            let verified = approval.signature.is_some()
                && checks
                    .signature
                    .is_some_and(|hook| hook.verify(&digest, approval));
            if !verified {
                missing_signature.get_or_insert_with(|| who.to_owned());
                continue;
            }
        }
        valid.push(who.to_owned());
    }

    let plan_changed = need.approvers > 0
        && !approvals.is_empty()
        && !approvals.iter().any(|a| a.plan_digest == digest);

    Evaluation {
        digest,
        risk,
        required: need.approvers,
        valid,
        rejected_by,
        expired,
        missing_signature,
        plan_changed,
    }
}

/// The only way to obtain an [`ApplyTicket`].
///
/// `approvals` may hold records for any plan; only those for this plan's exact digest count.
/// `now` is unix microseconds, passed in because this crate never reads a clock.
///
/// # Errors
/// A [`Refusal`] when the policy is invalid, the plan was modified after approval, it has too
/// few valid distinct approvals for its risk class, an approver rejected it, the approvals
/// expired, or a required hardware signature is missing.
pub fn authorise_apply(
    plan: &ReleasePlan,
    approvals: &[Approval],
    policy: &ApprovalPolicy,
    now: i64,
    checks: &Checks<'_>,
) -> Result<ApplyTicket, Refusal> {
    policy.validate().map_err(|e| Refusal::BadPolicy {
        message: e.to_string(),
    })?;
    let evaluation = evaluate(plan, approvals, policy, now, checks);
    if let Some(refusal) = evaluation.refusal() {
        return Err(refusal);
    }
    let approvers = if evaluation.required == 0 {
        Vec::new()
    } else {
        evaluation.valid
    };
    Ok(ApplyTicket::new(
        evaluation.digest,
        evaluation.risk,
        approvers,
        now,
    ))
}
