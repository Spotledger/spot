//! From a validated identity to a sign-in decision (ADR-070 sections 1.4, 6.1 and 7).
//!
//! A **known** linked identity signs in, provided the user is enabled and the tenant still
//! allows SSO. An **unknown** identity may, in order: be linked through an open invitation,
//! require the user to prove control of an existing local account (an e-mail match alone never
//! links — `sso.link_requires_proof`), create a user when a just-in-time rule matches (with no
//! roles), or be denied. A disabled user, an identity already linked to another user, and a
//! denied provider each have their own code.

use spot_iam_access::AuthMethod;
use spot_iam_directory::{
    IdentityKey, LinkedIdentity, SignInMethod, UserType, check_identity_unique,
    validate_sign_in_method,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::error::{self as e, Result, SsoError};
use crate::identity::SsoIdentity;
use crate::policy::{DirectoryView, SignInMode, SignInPolicy};

/// What to do with a validated identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Sign the linked user in, at SSO strength.
    SignIn {
        /// The user.
        user: RecordId,
        /// The session method: always [`AuthMethod::Sso`], never weaker than the policy.
        method: AuthMethod,
    },
    /// Link the identity to the user a pending invitation holds, then sign in.
    LinkViaInvitation {
        /// The invitation to accept.
        invitation: RecordId,
        /// The e-mail the invitation was sent to.
        email: String,
    },
    /// The e-mail matches a local account, but the user must prove control of it first.
    RequireLocalConfirmation {
        /// The existing local user.
        user: RecordId,
    },
    /// Create a user for the identity (just-in-time), with no roles.
    CreateUser {
        /// The kind of user to create.
        user_type: UserType,
        /// The identity to link to the new user.
        identity: Box<SsoIdentity>,
        /// The audit event the caller must journal.
        audit: JitCreated,
    },
    /// Refuse the sign-in.
    Denied(SsoError),
}

/// The audit event a just-in-time creation writes (ADR-070 section 7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitCreated {
    /// The provider the user arrived through.
    pub provider: crate::provider::ProviderId,
    /// The provider's subject for the new user.
    pub subject: String,
    /// The kind of user created.
    pub user_type: UserType,
}

/// Turns a validated identity into a sign-in decision.
#[must_use]
pub fn resolve(
    identity: &SsoIdentity,
    view: &impl DirectoryView,
    policy: &SignInPolicy,
    now: OffsetDateTime,
) -> Resolution {
    if let Some(user) = view.linked_user(&identity.issuer, &identity.subject) {
        return known_user(view, policy, user);
    }
    if policy.mode == SignInMode::Local {
        return Resolution::Denied(denied(
            e::codes::SSO_NOT_ALLOWED,
            "Single sign-on is not available for this tenant.",
            "Enable single sign-on in the tenant's sign-in policy first.",
        ));
    }
    // E-mail-based paths need a verified e-mail, so an unverified claim cannot link.
    if identity.email_verified
        && let Some(email) = identity.email.clone()
    {
        if let Some(invitation) = view.open_invitation(&email, now) {
            return Resolution::LinkViaInvitation { invitation, email };
        }
        if let Some(user) = view.user_for_email(&email) {
            return Resolution::RequireLocalConfirmation { user };
        }
    }
    if let Some(rule) = policy.jit.matching_rule(identity) {
        return Resolution::CreateUser {
            user_type: rule.user_type,
            identity: Box::new(identity.clone()),
            audit: JitCreated {
                provider: identity.provider.clone(),
                subject: identity.subject.clone(),
                user_type: rule.user_type,
            },
        };
    }
    Resolution::Denied(denied(
        e::codes::UNKNOWN_IDENTITY,
        "This account is not set up for single sign-on here.",
        "Invite the user, link their identity, or enable a just-in-time rule.",
    ))
}

/// The known-identity branch.
fn known_user(view: &impl DirectoryView, policy: &SignInPolicy, user: RecordId) -> Resolution {
    let Some(directory_user) = view.user(user) else {
        return Resolution::Denied(denied(
            e::codes::UNKNOWN_IDENTITY,
            "This account is not set up for single sign-on here.",
            "Check the linked identity points at an existing user.",
        ));
    };
    if !directory_user.enabled {
        return Resolution::Denied(denied(
            e::codes::USER_DISABLED,
            "This account is not available.",
            "Re-enable the user, or remove the linked identity.",
        ));
    }
    if policy.mode == SignInMode::Local || !directory_user.allows_sso {
        return Resolution::Denied(denied(
            e::codes::SSO_NOT_ALLOWED,
            "Single sign-on is not available for this account.",
            "Enable single sign-on for this user, or let them sign in locally.",
        ));
    }
    if let Err(_error) = validate_sign_in_method(directory_user.user_type, SignInMethod::Sso) {
        return Resolution::Denied(denied(
            e::codes::SSO_NOT_ALLOWED,
            "Single sign-on is not available for this account.",
            "Use the method this kind of user signs in with.",
        ));
    }
    Resolution::SignIn {
        user,
        method: AuthMethod::Sso,
    }
}

/// Checks a new linked identity against the existing ones, mapping the directory error.
///
/// # Errors
///
/// [`e::codes::IDENTITY_OTHER_USER`] when `(issuer, subject)` already belongs to another user.
pub fn link_identity(existing: &[LinkedIdentity], new: &LinkedIdentity) -> Result<()> {
    match check_identity_unique(existing, new) {
        Ok(()) => Ok(()),
        Err(_) => Err(denied(
            e::codes::IDENTITY_OTHER_USER,
            "This sign-in account is already connected to another user.",
            "Unlink it from the other user, or sign in as that user.",
        )),
    }
}

/// The `(issuer, subject)` key of an identity, as the directory stores it.
#[must_use]
pub fn identity_key(identity: &SsoIdentity) -> IdentityKey {
    identity.key()
}

fn denied(code: &'static str, public: &'static str, admin: &'static str) -> SsoError {
    SsoError::new(code, public, admin)
}
