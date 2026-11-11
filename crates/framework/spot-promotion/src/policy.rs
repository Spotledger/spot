//! Approval and rehearsal policy, per target environment.
//!
//! The migration approval crate decides *how many* people must approve a plan of a given risk.
//! This module says what a promotion to `dev`, `test` or `prod` is worth in those terms:
//! production needs two owners and a rehearsal; test auto-approves an additive bundle and needs
//! no rehearsal; development is the loosest. The requester of a production promotion never
//! counts as one of its approvers.
//!
//! Auto-approval covers additive plans only. A bundle that updates or removes something carries
//! a higher risk class, and the approval crate then asks for the environment's approvers: a
//! deletion is never waved through because the target happens to be `test`.

use spot_environments::Role;
use spot_migration_approval::{
    AdditiveRule, ApprovalPolicy, ApprovalStore, AuthorshipCheck, DestructiveRule, PlanDigest,
    ReviewRule,
};
use spot_migration_plan::Risk;

use crate::error::PromotionError;

/// One environment's approval and rehearsal policy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentPolicy {
    /// Distinct approvers needed for a plan the policy does not auto-approve (at least 1).
    pub approvers: u32,
    /// The role every approver must hold, when one is required.
    pub role: Option<String>,
    /// Whether an additive plan needs no approval.
    pub auto_approve: bool,
    /// Whether a rehearsal must pass before apply.
    pub require_rehearsal: bool,
    /// Whether every approval must carry a verified hardware signature.
    pub require_hardware_signature: bool,
    /// Whether the promotion's requester is barred from approving it.
    pub forbid_requester: bool,
}

impl EnvironmentPolicy {
    /// The `dev` policy: additive plans auto-approve, no rehearsal.
    #[must_use]
    pub fn dev() -> Self {
        Self {
            approvers: 0,
            role: None,
            auto_approve: true,
            require_rehearsal: false,
            require_hardware_signature: false,
            forbid_requester: false,
        }
    }

    /// The `test` policy: additive plans auto-approve, no rehearsal.
    #[must_use]
    pub fn test() -> Self {
        Self::dev()
    }

    /// The `prod` policy: two approvers in the `owner` role, the requester barred, rehearsal
    /// required.
    #[must_use]
    pub fn prod() -> Self {
        Self {
            approvers: 2,
            role: Some("owner".to_owned()),
            auto_approve: false,
            require_rehearsal: true,
            require_hardware_signature: false,
            forbid_requester: true,
        }
    }
}

/// The promotion policy of one environment set: one rule per role.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromotionPolicy {
    /// Development.
    pub dev: EnvironmentPolicy,
    /// Test.
    pub test: EnvironmentPolicy,
    /// Production.
    pub prod: EnvironmentPolicy,
    /// How long an approval stays valid, in microseconds (at least 1).
    pub expiry_micros: i64,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            dev: EnvironmentPolicy::dev(),
            test: EnvironmentPolicy::test(),
            prod: EnvironmentPolicy::prod(),
            expiry_micros: 86_400_000_000,
        }
    }
}

impl PromotionPolicy {
    /// The rule for one environment role.
    #[must_use]
    pub fn for_role(&self, role: Role) -> &EnvironmentPolicy {
        match role {
            Role::Dev => &self.dev,
            Role::Test => &self.test,
            Role::Prod => &self.prod,
        }
    }

    /// Whether a promotion to `role` must be rehearsed before apply.
    #[must_use]
    pub fn requires_rehearsal(&self, role: Role) -> bool {
        self.for_role(role).require_rehearsal
    }

    /// The approval policy `spot-migration-approval` evaluates for a promotion to `role`.
    ///
    /// Every risk class is mapped: additive plans follow `auto_approve`, review plans need the
    /// environment's approvers, and destructive plans need at least two (the approval crate's
    /// two-person floor).
    #[must_use]
    pub fn approval_policy(&self, role: Role) -> ApprovalPolicy {
        let env = self.for_role(role);
        ApprovalPolicy {
            additive: AdditiveRule {
                auto_approve: env.auto_approve,
                approvers: env.approvers.max(1),
                role: env.role.clone(),
            },
            needs_review: ReviewRule {
                approvers: env.approvers.max(1),
                role: env.role.clone(),
            },
            destructive: DestructiveRule {
                approvers: env.approvers.max(2),
                role: env.role.clone(),
                require_hardware_signature: env.require_hardware_signature,
            },
            expiry_micros: self.expiry_micros,
            forbid_author_approval: env.forbid_requester,
        }
    }

    /// Checks every role's policy is usable by the approval crate.
    ///
    /// # Errors
    /// [`PromotionError::PolicyInvalid`] when a destructive plan could be approved by fewer than
    /// two people, a review by nobody, or the expiry is not positive.
    pub fn validate(&self) -> Result<(), PromotionError> {
        for role in Role::ALL {
            self.approval_policy(role).validate().map_err(|error| {
                PromotionError::PolicyInvalid {
                    message: format!("{}: {error}", role.name()),
                }
            })?;
        }
        Ok(())
    }

    /// The approval requirement for a plan of `risk` promoted to `role`.
    #[must_use]
    pub fn requirement(&self, role: Role, risk: Risk) -> spot_migration_approval::Requirement {
        self.approval_policy(role).requirement(risk)
    }
}

/// The separation-of-duties check, answered from the approval store's persisted authorship: the
/// author is whoever the promotion registered when it planned, so it holds across a restart.
///
/// A store that cannot answer counts everybody as the author, which refuses the approval (fail
/// closed).
pub(crate) struct StoredAuthor<'a> {
    /// The store the plan's author was registered in.
    pub store: &'a dyn ApprovalStore,
}

impl AuthorshipCheck for StoredAuthor<'_> {
    fn is_author(&self, plan: &PlanDigest, who: &str) -> bool {
        match self.store.author_of(plan) {
            Ok(Some(author)) => author == who,
            Ok(None) | Err(_) => true,
        }
    }
}
