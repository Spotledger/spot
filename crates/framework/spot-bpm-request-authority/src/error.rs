//! The one error type, with stable `bpm_request_authority.*` codes.
//!
//! Every variant maps to one code and one [`ErrorClass`], so an edge that refuses a
//! grant-shaped command id hands back an [`AppError`] the native API envelope already
//! understands. No message carries a secret, a command id or vault text: an error is logged and
//! stored forever, and a command id can itself be a grant.

use spot_app_api::{AppError, ErrorClass};
use spot_bpm_governance::GovernanceError;

use crate::codes;

/// Everything this crate can refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// A client-supplied command id was shaped like a request grant.
    #[error(
        "A command id may not be shaped like a request grant: the authority issues grants, a \
         caller never supplies one."
    )]
    CommandIdReserved,

    /// A grant was asked for with a blank request, step or acting person.
    #[error("A request, a step and the acting person are all required to issue a grant.")]
    IdentifierBlank,

    /// The acting person is not a typed principal reference.
    #[error(
        "The acting person of a grant must be a typed principal reference: `user:<id>`, \
         `consumer:<id>` or `system`."
    )]
    ActorNotTyped,

    /// The requested expiry is in the past or beyond the cap.
    #[error(
        "A grant must expire in the future and at most {max_minutes} minutes after it is issued."
    )]
    LifetimeTooLong {
        /// The cap, in minutes.
        max_minutes: i64,
    },

    /// The request or step is not open at that attempt.
    #[error("The request or its step is not open at attempt {attempt}.")]
    AttemptNotCurrent {
        /// The attempt the grant was asked for.
        attempt: u32,
    },

    /// The server secret could not be resolved, or is too short to sign with.
    #[error("The server secret for request grants is unavailable.")]
    SecretUnavailable,
}

impl GrantError {
    /// The stable code this error carries.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CommandIdReserved => codes::COMMAND_ID_RESERVED,
            Self::IdentifierBlank => codes::IDENTIFIER_BLANK,
            Self::ActorNotTyped => codes::ACTOR_NOT_TYPED,
            Self::LifetimeTooLong { .. } => codes::LIFETIME_TOO_LONG,
            Self::AttemptNotCurrent { .. } => codes::ATTEMPT_NOT_CURRENT,
            Self::SecretUnavailable => codes::SECRET_UNAVAILABLE,
        }
    }

    /// The error class the code rides, matching the catalogue entry.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::CommandIdReserved
            | Self::IdentifierBlank
            | Self::ActorNotTyped
            | Self::LifetimeTooLong { .. } => ErrorClass::Validation,
            Self::AttemptNotCurrent { .. } => ErrorClass::InvalidState,
            Self::SecretUnavailable => ErrorClass::Internal,
        }
    }

    /// This error as the engine's [`AppError`], ready for an edge to return unchanged.
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        AppError::new(self.class(), self.code(), self.to_string())
    }

    /// This error as the [`RequestAuthority`](spot_bpm_governance::RequestAuthority) port's
    /// error, whose `issue` reserves one variant for every way an authority cannot issue.
    #[must_use]
    pub fn to_governance_error(&self) -> GovernanceError {
        GovernanceError::Settings {
            detail: self.to_string(),
        }
    }
}
