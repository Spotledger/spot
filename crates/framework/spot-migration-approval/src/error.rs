//! Typed errors and refusals for `spot-migration-approval` (stable `migration_approval.*`
//! codes). Callers match on `code()`, never on the message.

/// Stable codes of this crate (catalogued in `migration_approval.toml`).
pub mod codes {
    /// The approval policy is not valid.
    pub const BAD_POLICY: &str = "migration_approval.bad_policy";
    /// The approval was recorded against a plan that is not the one in hand.
    pub const PLAN_CHANGED: &str = "migration_approval.plan_changed";
    /// Not enough valid approvals for the plan's risk class.
    pub const NOT_ENOUGH_APPROVALS: &str = "migration_approval.not_enough_approvals";
    /// The plan was rejected.
    pub const REJECTED: &str = "migration_approval.rejected";
    /// Every approval has expired.
    pub const EXPIRED: &str = "migration_approval.expired";
    /// A destructive plan needs a hardware signature that is missing or does not verify.
    pub const SIGNATURE_REQUIRED: &str = "migration_approval.signature_required";
    /// The approver may not approve this plan (separation of duties or wrong role).
    pub const NOT_PERMITTED: &str = "migration_approval.not_permitted";
    /// The approval store or the audit sink failed.
    pub const BACKEND: &str = "migration_approval.backend";
    /// The audit record could not be written, so the action was not taken.
    pub const AUDIT_UNAVAILABLE: &str = "migration_approval.audit_unavailable";
    /// A CLI command line is not understood.
    pub const BAD_COMMAND: &str = "migration_approval.bad_command";
    /// The plan has no recorded author, so no approval is taken (fail closed).
    pub const AUTHOR_UNKNOWN: &str = "migration_approval.author_unknown";
    /// The plan is already registered with a different author.
    pub const AUTHOR_CONFLICT: &str = "migration_approval.author_conflict";
    /// The caller claimed a role their verified identity does not hold.
    pub const ROLE_NOT_HELD: &str = "migration_approval.role_not_held";
    /// A signing key is not bound to one approver, so it is not trusted for approvals.
    pub const KEY_NOT_BOUND: &str = "migration_approval.key_not_bound";
}

/// Why an approval action failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApprovalError {
    /// The policy is not valid.
    #[error("{}: {message}", codes::BAD_POLICY)]
    BadPolicy {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The approver may not act on this plan.
    #[error("{}: {reason}", codes::NOT_PERMITTED)]
    NotPermitted {
        /// Why, in a sentence.
        reason: String,
    },
    /// The store or the audit sink failed; nothing was recorded.
    #[error("{}: {message}", codes::BACKEND)]
    Backend {
        /// What failed, in a sentence (never a secret).
        message: String,
    },
    /// The audit record could not be written; the action was not taken (fail closed).
    #[error("{}: {message}", codes::AUDIT_UNAVAILABLE)]
    AuditUnavailable {
        /// What failed, in a sentence (never a secret).
        message: String,
    },
    /// A CLI command line is not understood.
    #[error("{}: {message}", codes::BAD_COMMAND)]
    BadCommand {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The plan has no recorded author; the approval is refused.
    #[error("{}: no author is recorded for this plan", codes::AUTHOR_UNKNOWN)]
    AuthorUnknown,
    /// The plan is already registered with a different author.
    #[error(
        "{}: this plan is already registered with another author",
        codes::AUTHOR_CONFLICT
    )]
    AuthorConflict,
    /// The claimed role is not one of the verified identity's roles.
    #[error("{}: the identity does not hold the role {role}", codes::ROLE_NOT_HELD)]
    RoleNotHeld {
        /// The role the caller claimed.
        role: String,
    },
    /// A trusted signing key is not bound to an approver.
    #[error(
        "{}: a trusted signing key is not bound to one approver",
        codes::KEY_NOT_BOUND
    )]
    KeyNotBound,
}

impl ApprovalError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPolicy { .. } => codes::BAD_POLICY,
            Self::NotPermitted { .. } => codes::NOT_PERMITTED,
            Self::Backend { .. } => codes::BACKEND,
            Self::AuditUnavailable { .. } => codes::AUDIT_UNAVAILABLE,
            Self::BadCommand { .. } => codes::BAD_COMMAND,
            Self::AuthorUnknown => codes::AUTHOR_UNKNOWN,
            Self::AuthorConflict => codes::AUTHOR_CONFLICT,
            Self::RoleNotHeld { .. } => codes::ROLE_NOT_HELD,
            Self::KeyNotBound => codes::KEY_NOT_BOUND,
        }
    }
}

/// Why `authorise_apply` refused to issue a ticket.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Refusal {
    /// There are approvals, but none for this exact plan: it was modified after approval.
    #[error(
        "{}: approvals exist only for a different version of this plan",
        codes::PLAN_CHANGED
    )]
    PlanChanged,
    /// The plan has fewer valid, distinct approvals than its risk class needs.
    #[error(
        "{}: {valid} valid approval(s), {required} required",
        codes::NOT_ENOUGH_APPROVALS
    )]
    NotEnoughApprovals {
        /// How many distinct valid approvers there are.
        valid: u32,
        /// How many the policy requires.
        required: u32,
    },
    /// An approver's latest decision is a rejection.
    #[error("{}: rejected by {by}", codes::REJECTED)]
    Rejected {
        /// The rejecting approver.
        by: String,
    },
    /// The approvals for this plan have all expired.
    #[error("{}: every approval has expired", codes::EXPIRED)]
    Expired,
    /// A destructive plan needs a verified hardware signature on each approval.
    #[error(
        "{}: {approver} has no verified hardware signature",
        codes::SIGNATURE_REQUIRED
    )]
    SignatureRequired {
        /// The approver whose signature is missing or invalid.
        approver: String,
    },
    /// The policy itself is invalid.
    #[error("{}: {message}", codes::BAD_POLICY)]
    BadPolicy {
        /// What is wrong, in a sentence.
        message: String,
    },
}

impl Refusal {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlanChanged => codes::PLAN_CHANGED,
            Self::NotEnoughApprovals { .. } => codes::NOT_ENOUGH_APPROVALS,
            Self::Rejected { .. } => codes::REJECTED,
            Self::Expired => codes::EXPIRED,
            Self::SignatureRequired { .. } => codes::SIGNATURE_REQUIRED,
            Self::BadPolicy { .. } => codes::BAD_POLICY,
        }
    }
}
