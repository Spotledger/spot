//! The one error of this crate: a list of problems, each with a stable `iam.*` code.
//!
//! Validation reports every problem at once, so a caller (an API, an import, a SCIM handler)
//! can show them together. Messages name the field or the rule, never a value: a sign-in name
//! or an address is personal data and does not belong in a log line.

use std::fmt;

/// `iam.invalid_sign_in_name`: the sign-in name is neither an address nor a username.
pub const INVALID_SIGN_IN_NAME: &str = "iam.invalid_sign_in_name";
/// `iam.invalid_display_name`: the display name is empty, too long or has control characters.
pub const INVALID_DISPLAY_NAME: &str = "iam.invalid_display_name";
/// `iam.party_link`: only a portal user has a party link.
pub const PARTY_LINK: &str = "iam.party_link";
/// `iam.manager_self`: a user cannot be their own manager.
pub const MANAGER_SELF: &str = "iam.manager_self";
/// `iam.invalid_state`: `enabled` and `deactivated_at` (or the timestamps) disagree.
pub const INVALID_STATE: &str = "iam.invalid_state";
/// `iam.reason_required`: a disable or enable needs a reason.
pub const REASON_REQUIRED: &str = "iam.reason_required";
/// `iam.last_admin`: the change would remove the last administrator of a kind.
pub const LAST_ADMIN: &str = "iam.last_admin";
/// `iam.break_glass_minimum`: SSO is required and fewer than two break-glass administrators
/// would remain.
pub const BREAK_GLASS_MINIMUM: &str = "iam.break_glass_minimum";
/// `iam.has_acted`: a user that has acted is never deleted.
pub const HAS_ACTED: &str = "iam.has_acted";
/// `iam.anonymised`: an anonymised user cannot be enabled again.
pub const ANONYMISED: &str = "iam.anonymised";
/// `iam.invalid_email`: an invitation's address is not an address.
pub const INVALID_EMAIL: &str = "iam.invalid_email";
/// `iam.invitation_type`: only internal and portal users are invited.
pub const INVITATION_TYPE: &str = "iam.invitation_type";
/// `iam.invitation_used`: the invitation was already accepted.
pub const INVITATION_USED: &str = "iam.invitation_used";
/// `iam.invitation_expired`: the invitation's time is over.
pub const INVITATION_EXPIRED: &str = "iam.invitation_expired";
/// `iam.invitation_cancelled`: the invitation was cancelled.
pub const INVITATION_CANCELLED: &str = "iam.invitation_cancelled";
/// `iam.invalid_identity`: a linked identity has no issuer or no subject, or too long a one.
pub const INVALID_IDENTITY: &str = "iam.invalid_identity";
/// `iam.identity_taken`: the `(issuer, subject)` pair is linked to another user.
pub const IDENTITY_TAKEN: &str = "iam.identity_taken";
/// `iam.portal_role`: a portal user can hold no desk role.
pub const PORTAL_ROLE: &str = "iam.portal_role";
/// `iam.no_password`: service and agent users do not sign in with a password.
pub const NO_PASSWORD: &str = "iam.no_password";
/// `iam.recovery_not_allowed`: the policy or the requester's standing does not allow it.
pub const RECOVERY_NOT_ALLOWED: &str = "iam.recovery_not_allowed";

/// One problem: a stable code, the field it is about when there is one, and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IamError {
    /// The stable, machine-readable code (`iam.*`).
    pub code: &'static str,
    /// The field the problem is about, when it is about one.
    pub field: Option<&'static str>,
    /// What is wrong, in a sentence that names no value.
    pub message: String,
}

impl IamError {
    /// A problem about no particular field.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            field: None,
            message: message.into(),
        }
    }

    /// A problem about `field`.
    pub fn in_field(code: &'static str, field: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            field: Some(field),
            message: message.into(),
        }
    }
}

impl fmt::Display for IamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.field {
            Some(field) => write!(f, "{} ({field}): {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

/// Every problem found, never empty.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct IamErrors(Vec<IamError>);

impl IamErrors {
    /// One problem.
    pub fn one(error: IamError) -> Self {
        Self(vec![error])
    }

    /// The problems, in the order found. `None` for an empty list, so an `Ok` is never
    /// mistaken for a failure and the reverse.
    pub fn from_list(list: Vec<IamError>) -> Option<Self> {
        (!list.is_empty()).then_some(Self(list))
    }

    /// The problems, in the order found.
    pub fn errors(&self) -> &[IamError] {
        &self.0
    }

    /// The codes, in the order found.
    pub fn codes(&self) -> Vec<&'static str> {
        self.0.iter().map(|e| e.code).collect()
    }

    /// Whether a problem with `code` is among them.
    pub fn has(&self, code: &str) -> bool {
        self.0.iter().any(|e| e.code == code)
    }
}

impl fmt::Display for IamErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, IamErrors>;
