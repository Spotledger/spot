//! The user directory's decision logic (WP-IAM-02.T1; ADR-070 sections 1 and 6).
//!
//! A pure, deterministic core: nothing here reads storage, the clock or any ambient state, and
//! nothing performs I/O. Every transition returns **what must happen next as data**, so the
//! later persistence, API and event tasks cannot get the lifecycle rules wrong.
//!
//! | Area | Where |
//! | --- | --- |
//! | Users, linked identities, invitations | [`User`], [`LinkedIdentity`], [`Invitation`] |
//! | Enable and disable, with the leaver actions | [`User::disable`], [`User::enable`], [`Transition`] |
//! | Last-administrator protection | [`check_admin_invariant`], [`DirectoryView`] |
//! | Anonymisation and deletion | [`anonymise`], [`check_delete`] |
//! | Recovery | [`required_steps`] |
//! | Portal and service rules | [`validate_assignment`], [`validate_assignment_kind`], [`validate_sign_in_method`] |
//!
//! Instants are passed in by the caller (from the platform `Clock`). No credential value, token
//! or secret appears in any type or message here; messages name fields and rules, never values.

mod admin;
mod anonymise;
mod error;
mod lifecycle;
mod model;
mod recovery;
mod rules;
mod wire;

pub use admin::{BREAK_GLASS_MINIMUM, Change, ChangeKind, DirectoryView, check_admin_invariant};
pub use anonymise::{
    ANONYMISED_DISPLAY_NAME, ANONYMISED_PREFIX, AnonymisedUser, UserHistory, anonymise,
    check_delete, is_anonymised,
};
pub use error::{IamError, IamErrors, Result};
pub use lifecycle::{AuditNote, LeaverAction, LifecycleChange, MAX_REASON, Transition, normalise};
pub use model::{
    IdentityKey, Invitation, InvitationState, LinkedIdentity, MAX_DISPLAY_NAME, MAX_IDENTITY_PART,
    MAX_SIGN_IN_NAME, User, UserType, check_identity_unique,
};
pub use recovery::{
    RecoveryKind, RecoveryPolicy, RecoveryRequest, Requester, Step, Target, required_steps,
};
pub use rules::{
    RoleKind, SignInMethod, validate_assignment, validate_assignment_kind, validate_sign_in_method,
};

/// The stable error codes of this crate, as listed in the error catalogue.
pub mod codes {
    pub use crate::error::{
        ANONYMISED, BREAK_GLASS_MINIMUM, HAS_ACTED, IDENTITY_TAKEN, INVALID_DISPLAY_NAME,
        INVALID_EMAIL, INVALID_IDENTITY, INVALID_SIGN_IN_NAME, INVALID_STATE, INVITATION_CANCELLED,
        INVITATION_EXPIRED, INVITATION_TYPE, INVITATION_USED, LAST_ADMIN, MANAGER_SELF,
        NO_PASSWORD, PARTY_LINK, PORTAL_ROLE, REASON_REQUIRED, RECOVERY_NOT_ALLOWED,
    };
}
