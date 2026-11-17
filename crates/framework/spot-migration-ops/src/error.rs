//! One error enum with stable `migration.*` codes.
//!
//! Messages never carry SQL text, values or paths: the only payload is an operation id from the
//! catalogue.

/// The stable codes.
pub mod codes {
    /// The actor lacks the `migration.run` permission (or is anonymous).
    pub const NOT_PERMITTED: &str = "migration.not_permitted";
    /// No stored plan has this hash.
    pub const UNKNOWN_PLAN: &str = "migration.unknown_plan";
    /// The plan's hash differs from the one asked for, or its state moved under the caller.
    pub const PLAN_CHANGED: &str = "migration.plan_changed";
    /// The plan has not been approved.
    pub const NOT_APPROVED: &str = "migration.not_approved";
    /// The approver is the plan's author.
    pub const SELF_APPROVAL: &str = "migration.self_approval";
    /// The plan has not been rehearsed.
    pub const NOT_REHEARSED: &str = "migration.not_rehearsed";
    /// The plan's risk class needs a review that has not happened.
    pub const RISK_REQUIRES_REVIEW: &str = "migration.risk_requires_review";
    /// Another migration or maintenance operation holds the site.
    pub const SITE_BUSY: &str = "migration.site_busy";
    /// A hold is active.
    pub const HOLD_ACTIVE: &str = "migration.hold_active";
    /// The apply failed.
    pub const APPLY_FAILED: &str = "migration.apply_failed";
    /// The rescue was refused.
    pub const RESCUE_REFUSED: &str = "migration.rescue_refused";
    /// The verb is not implemented yet (fail-closed stub).
    pub const NOT_IMPLEMENTED: &str = "migration.not_implemented";
    /// The free space is below 2.5 times the snapshot size (rehearsal refuses to start).
    pub const INSUFFICIENT_SPACE: &str = "migration.insufficient_space";
    /// A rehearsal failed or ran out of its time budget (the clone was cleaned up).
    pub const REHEARSAL_FAILED: &str = "migration.rehearsal_failed";
    /// A port the verb needs is not wired in this host.
    pub const PORT_UNAVAILABLE: &str = "migration.port_unavailable";
    /// A store (plan, approval, run) failed.
    pub const STORE_FAILED: &str = "migration.store_failed";
}

/// Management API error code for a refusal by policy or permission.
pub const MGMT_REFUSED_BY_POLICY: &str = "mgmt.refused_by_policy";
/// Management API error code for an internal failure.
pub const MGMT_INTERNAL: &str = "mgmt.internal";
/// Exit code of a refusal by policy or permission.
pub const EXIT_REFUSED: u8 = 2;
/// Exit code of a failure.
pub const EXIT_FAILED: u8 = 3;

/// Why a lifecycle operation did not run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    /// `migration.not_permitted`.
    #[error("migration.not_permitted: the actor may not run migrations")]
    NotPermitted,
    /// `migration.unknown_plan`.
    #[error("migration.unknown_plan: no plan has this hash")]
    UnknownPlan,
    /// `migration.plan_changed`.
    #[error("migration.plan_changed: the plan changed since it was read")]
    PlanChanged,
    /// `migration.not_approved`.
    #[error("migration.not_approved: the plan is not approved")]
    NotApproved,
    /// `migration.self_approval`.
    #[error("migration.self_approval: the approver must differ from the plan author")]
    SelfApproval,
    /// `migration.not_rehearsed`.
    #[error("migration.not_rehearsed: the plan has not been rehearsed")]
    NotRehearsed,
    /// `migration.risk_requires_review`.
    #[error("migration.risk_requires_review: the plan's risk class needs a review")]
    RiskRequiresReview,
    /// `migration.site_busy`.
    #[error("migration.site_busy: the site is busy with another operation")]
    SiteBusy,
    /// `migration.hold_active`.
    #[error("migration.hold_active: a hold is active")]
    HoldActive,
    /// `migration.apply_failed`.
    #[error("migration.apply_failed: the apply did not complete")]
    ApplyFailed,
    /// `migration.rescue_refused`.
    #[error("migration.rescue_refused: the rescue was refused")]
    RescueRefused,
    /// `migration.not_implemented`: the fail-closed stub.
    #[error("migration.not_implemented: operation {op} is not implemented yet")]
    NotImplemented {
        /// The catalogue operation id.
        op: String,
    },
    /// `migration.insufficient_space`.
    #[error("migration.insufficient_space: there is not enough free space to rehearse")]
    InsufficientSpace,
    /// `migration.rehearsal_failed`.
    #[error("migration.rehearsal_failed: the rehearsal did not complete")]
    RehearsalFailed,
    /// `migration.port_unavailable`: the host did not wire a port the verb needs.
    #[error("migration.port_unavailable: the {port} port is not wired in this host")]
    PortUnavailable {
        /// The port name (a fixed word).
        port: &'static str,
    },
    /// `migration.store_failed`.
    #[error("migration.store_failed: a lifecycle store failed")]
    Store,
}

impl LifecycleError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotPermitted => codes::NOT_PERMITTED,
            Self::UnknownPlan => codes::UNKNOWN_PLAN,
            Self::PlanChanged => codes::PLAN_CHANGED,
            Self::NotApproved => codes::NOT_APPROVED,
            Self::SelfApproval => codes::SELF_APPROVAL,
            Self::NotRehearsed => codes::NOT_REHEARSED,
            Self::RiskRequiresReview => codes::RISK_REQUIRES_REVIEW,
            Self::SiteBusy => codes::SITE_BUSY,
            Self::HoldActive => codes::HOLD_ACTIVE,
            Self::ApplyFailed => codes::APPLY_FAILED,
            Self::RescueRefused => codes::RESCUE_REFUSED,
            Self::NotImplemented { .. } => codes::NOT_IMPLEMENTED,
            Self::InsufficientSpace => codes::INSUFFICIENT_SPACE,
            Self::RehearsalFailed => codes::REHEARSAL_FAILED,
            Self::PortUnavailable { .. } => codes::PORT_UNAVAILABLE,
            Self::Store => codes::STORE_FAILED,
        }
    }

    /// The process exit code: 2 refused by policy or permission, 3 failed.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::ApplyFailed
            | Self::RehearsalFailed
            | Self::Store
            | Self::PortUnavailable { .. } => EXIT_FAILED,
            _ => EXIT_REFUSED,
        }
    }

    /// The management API error code (`mgmt.refused_by_policy` or `mgmt.internal`).
    #[must_use]
    pub fn mgmt_code(&self) -> &'static str {
        match self {
            Self::ApplyFailed
            | Self::RehearsalFailed
            | Self::Store
            | Self::PortUnavailable { .. } => MGMT_INTERNAL,
            _ => MGMT_REFUSED_BY_POLICY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<LifecycleError> {
        vec![
            LifecycleError::NotPermitted,
            LifecycleError::UnknownPlan,
            LifecycleError::PlanChanged,
            LifecycleError::NotApproved,
            LifecycleError::SelfApproval,
            LifecycleError::NotRehearsed,
            LifecycleError::RiskRequiresReview,
            LifecycleError::SiteBusy,
            LifecycleError::HoldActive,
            LifecycleError::ApplyFailed,
            LifecycleError::RescueRefused,
            LifecycleError::NotImplemented {
                op: "site.migrate.plan".to_owned(),
            },
            LifecycleError::InsufficientSpace,
            LifecycleError::RehearsalFailed,
            LifecycleError::PortUnavailable { port: "runs" },
            LifecycleError::Store,
        ]
    }

    #[test]
    fn codes_are_stable_and_prefix_the_message() {
        for error in all() {
            assert!(error.code().starts_with("migration."));
            assert!(error.to_string().starts_with(error.code()), "{error}");
        }
        assert_eq!(LifecycleError::PlanChanged.code(), "migration.plan_changed");
        assert_eq!(
            LifecycleError::SelfApproval.code(),
            "migration.self_approval"
        );
    }

    #[test]
    fn codes_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for error in all() {
            assert!(seen.insert(error.code()), "{}", error.code());
        }
    }

    #[test]
    fn exit_and_management_codes() {
        for error in all() {
            match error {
                LifecycleError::ApplyFailed
                | LifecycleError::RehearsalFailed
                | LifecycleError::Store
                | LifecycleError::PortUnavailable { .. } => {
                    assert_eq!(error.exit_code(), 3);
                    assert_eq!(error.mgmt_code(), "mgmt.internal");
                }
                _ => {
                    assert_eq!(error.exit_code(), 2);
                    assert_eq!(error.mgmt_code(), "mgmt.refused_by_policy");
                }
            }
        }
    }
}
