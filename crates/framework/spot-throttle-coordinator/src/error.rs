//! The throttle's errors: stable `throttle.*` codes, and messages that name no raw value.

use std::fmt;
use std::time::Duration;

/// The coordinator failed, so the throttle cannot count: refuse the attempt (fail closed).
pub const UNAVAILABLE: &str = "throttle_coordinator.unavailable";

/// A verdict of `Deny`, turned into an error by [`crate::Verdict::require_allowed`].
pub const RATE_LIMITED: &str = "throttle_coordinator.rate_limited";

/// The policy or the salt was rejected at construction.
pub const BAD_POLICY: &str = "throttle_coordinator.bad_policy";

/// The attempt cannot be counted: a missing field its kind needs, an empty or over-long value,
/// a tenant that is not a plain id, or a source with neither an address nor a connection id.
pub const BAD_ATTEMPT: &str = "throttle_coordinator.bad_attempt";

/// The salt could not be obtained from its source (WP-IAM-11.T12), so nothing can be counted:
/// refuse the attempt (fail closed).
pub const SALT_UNAVAILABLE: &str = "throttle_coordinator.salt_unavailable";

/// A rotation was refused by [`crate::SaltRing::rotate_checked`] (WP-IAM-11.T12): it would drop
/// a previous salt that has not finished its window, or it names the salt already in use.
pub const BAD_ROTATION: &str = "throttle_coordinator.bad_rotation";

/// What stopped one throttle call.
///
/// Every message is a fixed sentence or carries only a stable code: never an account name, an
/// address, an e-mail, a phone number or a bucket key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThrottleError {
    /// The coordinator failed (`cause` is its stable `coordinator.*` code). Callers must treat
    /// this as **refuse**; the throttle never answers `Allow` after it.
    Unavailable {
        /// The coordinator's error code.
        cause: &'static str,
    },
    /// The attempt was denied; wait `retry_after`.
    RateLimited {
        /// How long until the denying bucket has refilled enough.
        retry_after: Duration,
    },
    /// The policy or the salt is unusable.
    BadPolicy {
        /// What is wrong, as a fixed sentence.
        reason: &'static str,
    },
    /// The attempt is malformed and was refused without counting.
    BadAttempt {
        /// What is wrong, as a fixed sentence.
        reason: &'static str,
    },
    /// The salt could not be obtained from its [`crate::SaltSource`]. Callers must treat this
    /// as **refuse**: the throttle never answers `Allow` without a salt.
    SaltUnavailable {
        /// What went wrong, as a fixed sentence that names no key and no tenant.
        reason: &'static str,
    },
    /// A rotation was refused; the ring is unchanged. Retire the previous salt first (see
    /// [`crate::SaltRing::safe_to_retire`]) or rotate to a salt that is actually new.
    BadRotation {
        /// What is wrong, as a fixed sentence that names no key.
        reason: &'static str,
    },
}

impl ThrottleError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => UNAVAILABLE,
            Self::RateLimited { .. } => RATE_LIMITED,
            Self::BadPolicy { .. } => BAD_POLICY,
            Self::BadAttempt { .. } => BAD_ATTEMPT,
            Self::SaltUnavailable { .. } => SALT_UNAVAILABLE,
            Self::BadRotation { .. } => BAD_ROTATION,
        }
    }
}

impl fmt::Display for ThrottleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { cause } => write!(
                f,
                "{UNAVAILABLE}: the sign-in throttle could not count (coordinator: {cause}); refuse"
            ),
            Self::RateLimited { retry_after } => {
                write!(f, "{RATE_LIMITED}: try again in {retry_after:?}")
            }
            Self::BadPolicy { reason } => write!(f, "{BAD_POLICY}: {reason}"),
            Self::BadAttempt { reason } => write!(f, "{BAD_ATTEMPT}: {reason}"),
            Self::SaltUnavailable { reason } => {
                write!(f, "{SALT_UNAVAILABLE}: {reason}; refuse")
            }
            Self::BadRotation { reason } => write!(f, "{BAD_ROTATION}: {reason}"),
        }
    }
}

impl std::error::Error for ThrottleError {}
