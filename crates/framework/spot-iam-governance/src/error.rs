//! The one error type, with stable, machine-readable `iam_governance.*` codes.
//!
//! Codes are stable: for logs, metrics and API responses use [`GovError::code`], never the
//! message text. Every error names the subject (the request, the role or the assignment) it
//! is about, so a caller fixing a refused grant sees which grant each problem belongs to.

/// Something failed while building, evaluating or recording a grant.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GovError {
    /// A grant request, a grant-request id or a record is malformed: a blank id, user, role
    /// or name where the model needs one — or a policy that requires approval but names no
    /// valid escalation target.
    #[error("{subject}: {detail}")]
    RequestInvalid {
        /// What the problem is about (the request id, or `(request)`).
        subject: String,
        /// What is wrong and what the model needs.
        detail: String,
    },

    /// The grantor may not grant the requested role or scope: a grantor grants only roles and
    /// scopes they hold themselves (ADR-070 section 10, rule 3).
    #[error("{role}: {detail}")]
    DelegationDenied {
        /// The role the grant was refused for.
        role: String,
        /// Why the grantor may not grant it.
        detail: String,
    },

    /// The role is managed by an identity provider, so a manual grant cannot touch it
    /// (ADR-070 section 4): change the user's group there, not here.
    #[error("{role}: {detail}")]
    IdpManaged {
        /// The IdP-managed role.
        role: String,
        /// Which provider manages it.
        detail: String,
    },

    /// The override submission is incomplete: an override needs a non-blank reason and a
    /// mitigating control with an id and text (ADR-070 section 8, rule 1).
    #[error("{subject}: {detail}")]
    OverrideIncomplete {
        /// The request or rule the override is about.
        subject: String,
        /// Which part of the override is missing.
        detail: String,
    },

    /// The grantor does not hold the `override_sod` right, so they cannot record an override.
    #[error("{user}: {detail}")]
    OverrideNotPermitted {
        /// The grantor who tried to override.
        user: String,
        /// What right is missing.
        detail: String,
    },

    /// The approval Process could not be built from the built-in template, or the launcher
    /// refused it.
    #[error("the grant approval process failed: {detail}")]
    ApprovalProcess {
        /// What the process builder or the launcher objected to.
        detail: String,
    },

    /// A decision arrived for an approval that is not pending (already decided or expired).
    #[error("{request}: {detail}")]
    ApprovalNotPending {
        /// The grant request the decision is about.
        request: String,
        /// The approval's current state.
        detail: String,
    },

    /// The person asked to approve is the person the grant is for: an approver is never the
    /// grantee.
    #[error("{request}: {detail}")]
    ApproverIsGrantee {
        /// The grant request.
        request: String,
        /// The user who is both approver and grantee.
        detail: String,
    },

    /// The person deciding is not the approver the approval names.
    #[error("{request}: {detail}")]
    NotTheApprover {
        /// The grant request.
        request: String,
        /// Who tried to decide and who is named.
        detail: String,
    },

    /// The grant cannot take effect because its approval is not approved: a rejected or
    /// expired approval leaves nothing granted.
    #[error("{request}: {detail}")]
    NotApproved {
        /// The grant request.
        request: String,
        /// The approval's current state.
        detail: String,
    },

    /// A canonical before/after snapshot could not be serialised.
    #[error("the change snapshot failed: {detail}")]
    SnapshotFailed {
        /// What the serialiser objected to.
        detail: String,
    },
}

/// The stable machine-readable code of an error, for logs, API responses and metrics.
impl GovError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `iam_governance.request_invalid` — a blank id, user, role or name, or a policy that
    ///   requires approval without a valid escalation target;
    /// - `iam_governance.delegation_denied` — the grantor may not grant the role or scope;
    /// - `iam_governance.idp_managed` — a manual grant cannot touch an IdP-managed role;
    /// - `iam_governance.override_incomplete` — an override reason or control is missing;
    /// - `iam_governance.override_not_permitted` — the grantor lacks `override_sod`;
    /// - `iam_governance.approval_process` — the approval Process could not be built/launched;
    /// - `iam_governance.approval_not_pending` — a decision on a non-pending approval;
    /// - `iam_governance.approver_is_grantee` — the approver is the grantee;
    /// - `iam_governance.not_approved` — the approval is not approved, so nothing is granted;
    /// - `iam_governance.snapshot_failed` — a before/after snapshot could not be serialised.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequestInvalid { .. } => REQUEST_INVALID,
            Self::DelegationDenied { .. } => DELEGATION_DENIED,
            Self::IdpManaged { .. } => IDP_MANAGED,
            Self::OverrideIncomplete { .. } => OVERRIDE_INCOMPLETE,
            Self::OverrideNotPermitted { .. } => OVERRIDE_NOT_PERMITTED,
            Self::ApprovalProcess { .. } => APPROVAL_PROCESS,
            Self::ApprovalNotPending { .. } => APPROVAL_NOT_PENDING,
            Self::ApproverIsGrantee { .. } => APPROVER_IS_GRANTEE,
            Self::NotTheApprover { .. } => NOT_THE_APPROVER,
            Self::NotApproved { .. } => NOT_APPROVED,
            Self::SnapshotFailed { .. } => SNAPSHOT_FAILED,
        }
    }

    /// The subject (request, role or assignment) the error is about, for grouping problems.
    #[must_use]
    pub fn subject(&self) -> &str {
        match self {
            Self::RequestInvalid { subject, .. }
            | Self::OverrideIncomplete { subject, .. }
            | Self::ApprovalNotPending {
                request: subject, ..
            }
            | Self::ApproverIsGrantee {
                request: subject, ..
            }
            | Self::NotTheApprover {
                request: subject, ..
            }
            | Self::NotApproved {
                request: subject, ..
            } => subject,
            Self::DelegationDenied { role, .. } | Self::IdpManaged { role, .. } => role,
            Self::OverrideNotPermitted { user, .. } => user,
            Self::ApprovalProcess { .. } | Self::SnapshotFailed { .. } => "",
        }
    }
}

/// A blank id, user, role or name.
pub const REQUEST_INVALID: &str = "iam_governance.request_invalid";
/// The grantor may not grant the role or scope.
pub const DELEGATION_DENIED: &str = "iam_governance.delegation_denied";
/// A manual grant cannot touch an IdP-managed role.
pub const IDP_MANAGED: &str = "iam_governance.idp_managed";
/// An override reason or mitigating control is missing.
pub const OVERRIDE_INCOMPLETE: &str = "iam_governance.override_incomplete";
/// The grantor does not hold the `override_sod` right.
pub const OVERRIDE_NOT_PERMITTED: &str = "iam_governance.override_not_permitted";
/// The approval Process could not be built or launched.
pub const APPROVAL_PROCESS: &str = "iam_governance.approval_process";
/// A decision arrived for an approval that is not pending.
pub const APPROVAL_NOT_PENDING: &str = "iam_governance.approval_not_pending";
/// The approver is the grantee.
pub const APPROVER_IS_GRANTEE: &str = "iam_governance.approver_is_grantee";
/// The person deciding is not the named approver.
pub const NOT_THE_APPROVER: &str = "iam_governance.not_the_approver";
/// The approval is not approved, so nothing is granted.
pub const NOT_APPROVED: &str = "iam_governance.not_approved";
/// A canonical before/after snapshot could not be serialised.
pub const SNAPSHOT_FAILED: &str = "iam_governance.snapshot_failed";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> Vec<(GovError, &'static str)> {
        vec![
            (
                GovError::RequestInvalid {
                    subject: "(request)".to_owned(),
                    detail: "needs a user".to_owned(),
                },
                REQUEST_INVALID,
            ),
            (
                GovError::DelegationDenied {
                    role: "Finance Approver".to_owned(),
                    detail: "not held".to_owned(),
                },
                DELEGATION_DENIED,
            ),
            (
                GovError::IdpManaged {
                    role: "Sales User".to_owned(),
                    detail: "provider okta".to_owned(),
                },
                IDP_MANAGED,
            ),
            (
                GovError::OverrideIncomplete {
                    subject: "r1".to_owned(),
                    detail: "needs a control".to_owned(),
                },
                OVERRIDE_INCOMPLETE,
            ),
            (
                GovError::OverrideNotPermitted {
                    user: "ana".to_owned(),
                    detail: "needs override_sod".to_owned(),
                },
                OVERRIDE_NOT_PERMITTED,
            ),
            (
                GovError::ApprovalProcess {
                    detail: "bad template".to_owned(),
                },
                APPROVAL_PROCESS,
            ),
            (
                GovError::ApprovalNotPending {
                    request: "r1".to_owned(),
                    detail: "approved".to_owned(),
                },
                APPROVAL_NOT_PENDING,
            ),
            (
                GovError::ApproverIsGrantee {
                    request: "r1".to_owned(),
                    detail: "ben".to_owned(),
                },
                APPROVER_IS_GRANTEE,
            ),
            (
                GovError::NotTheApprover {
                    request: "r1".to_owned(),
                    detail: "mallory".to_owned(),
                },
                NOT_THE_APPROVER,
            ),
            (
                GovError::NotApproved {
                    request: "r1".to_owned(),
                    detail: "expired".to_owned(),
                },
                NOT_APPROVED,
            ),
            (
                GovError::SnapshotFailed {
                    detail: "not serialisable".to_owned(),
                },
                SNAPSHOT_FAILED,
            ),
        ]
    }

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        for (error, code) in sample() {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }

    #[test]
    fn every_error_names_its_subject() {
        for (error, _) in sample() {
            // The two process-level errors have no single subject; the rest must name one.
            if matches!(
                error,
                GovError::ApprovalProcess { .. } | GovError::SnapshotFailed { .. }
            ) {
                continue;
            }
            assert!(!error.subject().is_empty(), "{error:?} names nothing");
        }
    }
}
