//! Recovery rules (ADR-070 section 6.5): who may reset a password or a second factor, and what
//! the reset must be wrapped in. A pure policy: no credential value appears anywhere here.
//!
//! - **Self-service** (the user resets their own) is allowed when the policy says so for that
//!   kind of reset. No extra step beyond the proof the host already demands.
//! - **Administrator-initiated** resets need a step-up of the administrator, and notify the
//!   user on every address on file.
//! - **Resetting an administrator** needs a second approver on top, and always notifies every
//!   address. An administrator's own self-service reset needs no approver (nobody else may be
//!   at hand) but still notifies every address, because taking over an administrator is the
//!   attack that matters.
//! - A **disabled** user, and a **service** or **agent** user (no interactive sign-in), cannot be
//!   recovered.

use spot_storage_contract::RecordId;

use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::UserType;

/// What is being reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryKind {
    /// The password.
    Password,
    /// The second factor.
    SecondFactor,
}

/// A request to recover access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryRequest {
    /// The user whose access is recovered.
    pub target: RecordId,
    /// The user who asks (the same id for self-service).
    pub initiated_by: RecordId,
    /// What is reset.
    pub kind: RecoveryKind,
}

impl RecoveryRequest {
    /// Whether the user resets their own.
    pub fn is_self_service(&self) -> bool {
        self.target == self.initiated_by
    }
}

/// The tenant's recovery policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPolicy {
    /// Whether a user may reset their own password.
    pub self_service_password: bool,
    /// Whether a user may reset their own second factor.
    pub self_service_second_factor: bool,
}

impl RecoveryPolicy {
    fn allows_self_service(&self, kind: RecoveryKind) -> bool {
        match kind {
            RecoveryKind::Password => self.self_service_password,
            RecoveryKind::SecondFactor => self.self_service_second_factor,
        }
    }
}

/// Who asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Requester {
    /// Whether they are a tenant administrator now.
    pub is_administrator: bool,
}

/// Whose access is recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The kind of user.
    pub user_type: UserType,
    /// Whether the user is enabled.
    pub enabled: bool,
    /// Whether they are a tenant administrator.
    pub is_administrator: bool,
}

/// A step the reset must be wrapped in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// The initiating administrator re-authenticates first.
    StepUp,
    /// A second administrator, who is neither the initiator nor the target, approves.
    SecondApprover,
    /// The user is told on every address on file.
    NotifyAllAddresses,
}

/// The steps a recovery needs, in the order they happen, or why it is refused.
pub fn required_steps(
    policy: &RecoveryPolicy,
    request: &RecoveryRequest,
    requester: &Requester,
    target: &Target,
) -> Result<Vec<Step>> {
    if !target.user_type.may_use_password() {
        return Err(IamErrors::one(IamError::new(
            e::NO_PASSWORD,
            "service and agent users do not sign in with a password or a second factor",
        )));
    }
    if !target.enabled {
        return Err(IamErrors::one(IamError::new(
            e::RECOVERY_NOT_ALLOWED,
            "a disabled user cannot recover access; enable the user first",
        )));
    }
    if request.is_self_service() {
        if !policy.allows_self_service(request.kind) {
            return Err(IamErrors::one(IamError::new(
                e::RECOVERY_NOT_ALLOWED,
                "the policy does not allow self-service recovery of this kind; ask an administrator",
            )));
        }
        return Ok(if target.is_administrator {
            vec![Step::NotifyAllAddresses]
        } else {
            Vec::new()
        });
    }
    if !requester.is_administrator {
        return Err(IamErrors::one(IamError::new(
            e::RECOVERY_NOT_ALLOWED,
            "only an administrator may reset another user's access",
        )));
    }
    Ok(if target.is_administrator {
        vec![Step::StepUp, Step::SecondApprover, Step::NotifyAllAddresses]
    } else {
        vec![Step::StepUp, Step::NotifyAllAddresses]
    })
}
