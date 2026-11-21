//! The one error type for session operations, with stable `session.*` codes.
//!
//! The caller-visible rule (brief, and the native API's
//! `auth.invalid_credentials`/`auth.session_expired` mapping): a validation failure never
//! distinguishes *why* a token failed. Unknown token, expired session and revoked session
//! are **one** error, [`SessionError::Invalid`], with its internal reason attached for the
//! audit log — the reason travels to logs, never to the client. Everything else is an
//! input problem (malformed token text) or a store failure, which carry their own codes.

use std::fmt;

/// Why a `validate` failed, for the internal (audit) part of [`SessionError::Invalid`].
///
/// Never serialised to a client in its own right; its text is for logs and identity
/// events. The variants exist so the audit trail can answer *which of the three reasons*
/// ended the session while the caller still sees only "invalid".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InternalReason {
    /// No digest-matching session exists. (This also covers a token presented in a valid
    /// shape that simply never belonged to this site.)
    UnknownDigest,
    /// The session was revoked.
    Revoked(SessionEndReasonText),
    /// Past the idle bound (half-open: at the bound is already expired).
    IdleExpired,
    /// Past the absolute bound.
    AbsoluteExpired,
}

/// The revocation reason text carried in [`InternalReason::Revoked`] for the audit log.
/// (The enum lives here rather than re-exporting the model type, so the error type stays
/// cheap to hold in logs.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEndReasonText(pub String);

impl fmt::Display for SessionEndReasonText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A session operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The presented token is unusable. One code for the caller; the internal reason says
    /// (only to logs) whether the token was unknown, revoked or expired.
    #[error("session.invalid: the session token is not valid (internal: {reason:?})")]
    Invalid {
        /// The internal reason, for audit only.
        reason: Box<InternalReason>,
    },

    /// The presented token does not have the shape `spot-auth`'s [`OpaqueToken`] accepts;
    /// refused before any digest is computed. (The API layer maps this to
    /// `auth.invalid_credentials` too — a malformed token is simply invalid.)
    #[error("session.malformed: the presented token does not have the expected shape")]
    Malformed,

    /// The store could not complete the operation. Message text is the store's own
    /// report; the code is stable.
    #[error("session.store_failed: {0}")]
    StoreFailed(String),
}

impl SessionError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "session.invalid",
            Self::Malformed => "session.malformed",
            Self::StoreFailed(_) => "session.store_failed",
        }
    }

    /// The internal reason, for the audit log of a `session.invalid`.
    #[must_use]
    pub fn internal_reason(&self) -> Option<&InternalReason> {
        match self {
            Self::Invalid { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Builds [`SessionError::Invalid`] with a boxed reason (the crate's own call sites).
pub(crate) fn invalid(reason: InternalReason) -> SessionError {
    SessionError::Invalid {
        reason: Box::new(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(
            invalid(InternalReason::UnknownDigest).code(),
            "session.invalid"
        );
        assert_eq!(
            invalid(InternalReason::IdleExpired).internal_reason(),
            Some(&InternalReason::IdleExpired)
        );
        assert_eq!(SessionError::Malformed.code(), "session.malformed");
        assert_eq!(
            SessionError::StoreFailed("down".to_owned()).code(),
            "session.store_failed"
        );
        assert!(SessionError::Malformed.internal_reason().is_none());
    }
}
