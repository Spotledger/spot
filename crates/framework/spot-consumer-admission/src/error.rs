//! The stable `consumer_admission.*` codes and the error type that carries them.
//!
//! Every message is a fixed sentence or a stable code: no destination, account name, source
//! address or bucket key is ever inside one.

use std::fmt;
use std::time::Duration;

use spot_throttle_coordinator::{SALT_UNAVAILABLE, ThrottleError};

/// The destination is not something this crate will canonicalise: an empty value, a non-ASCII
/// local part or domain, a phone without a leading `+` or with the wrong number of digits.
pub const BAD_DESTINATION: &str = "consumer_admission.bad_destination";

/// The throttle could not count (the coordinator failed or the salt is unavailable), so the
/// attempt is refused. Callers must treat this as **refuse**: it is never a quiet allow.
pub const UNAVAILABLE: &str = "consumer_admission.unavailable";

/// The attempt was refused; wait `retry_after` before the same attempt can succeed.
pub const RATE_LIMITED: &str = "consumer_admission.rate_limited";

/// The admission policy is unusable: a cost of zero or a cooldown that cannot be armed.
pub const BAD_POLICY: &str = "consumer_admission.bad_policy";

/// The request cannot be priced: a tenant that is not a plain id, or a purpose missing the
/// account name or destination it counts.
pub const BAD_REQUEST: &str = "consumer_admission.bad_request";

/// What stopped one admission call.
///
/// `Err` from [`crate::ConsumerAdmission::admit`] always means **refuse the attempt**. The
/// variants carry a fixed sentence, a stable code or a wait — never a value a caller supplied.
#[derive(Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// The destination could not be canonicalised; nothing was counted.
    BadDestination {
        /// What is wrong, as a fixed sentence.
        reason: &'static str,
    },
    /// The coordinator (or the salt) failed, so nothing could be counted. Refuse.
    Unavailable {
        /// The stable code of what failed (`throttle_coordinator.*`).
        cause: &'static str,
    },
    /// A bucket refused the attempt; the same attempt can succeed after `retry_after`.
    RateLimited {
        /// How long until the denying bucket (or the resend cooldown) has refilled enough.
        retry_after: Duration,
    },
    /// The policy or the salt it needs is unusable; the service is misconfigured.
    BadPolicy {
        /// What is wrong, as a fixed sentence.
        reason: &'static str,
    },
    /// The request is missing what its purpose counts, or names a tenant that is not a plain id.
    BadRequest {
        /// What is wrong, as a fixed sentence.
        reason: &'static str,
    },
}

impl AdmissionError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadDestination { .. } => BAD_DESTINATION,
            Self::Unavailable { .. } => UNAVAILABLE,
            Self::RateLimited { .. } => RATE_LIMITED,
            Self::BadPolicy { .. } => BAD_POLICY,
            Self::BadRequest { .. } => BAD_REQUEST,
        }
    }

    /// The wait the store asked for, when the error is a refusal by a bucket.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    /// Turns a throttle error into the code a consumer front door answers with.
    ///
    /// A coordinator failure and a missing salt are both [`AdmissionError::Unavailable`]: the
    /// attempt was not counted, so the caller refuses it.
    pub(crate) fn from_throttle(error: ThrottleError) -> Self {
        match error {
            ThrottleError::Unavailable { cause } => Self::Unavailable { cause },
            ThrottleError::RateLimited { retry_after } => Self::RateLimited { retry_after },
            ThrottleError::BadPolicy { reason } => Self::BadPolicy { reason },
            ThrottleError::BadAttempt { reason } => Self::BadRequest { reason },
            ThrottleError::SaltUnavailable { .. } => Self::Unavailable {
                cause: SALT_UNAVAILABLE,
            },
            ThrottleError::BadRotation { reason } => Self::BadPolicy { reason },
        }
    }
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadDestination { reason } => write!(f, "{BAD_DESTINATION}: {reason}"),
            Self::Unavailable { cause } => write!(
                f,
                "{UNAVAILABLE}: the attempt could not be counted ({cause}); refuse"
            ),
            Self::RateLimited { retry_after } => {
                write!(f, "{RATE_LIMITED}: try again in {retry_after:?}")
            }
            Self::BadPolicy { reason } => write!(f, "{BAD_POLICY}: {reason}"),
            Self::BadRequest { reason } => write!(f, "{BAD_REQUEST}: {reason}"),
        }
    }
}

impl fmt::Debug for AdmissionError {
    /// Same shape as `Display`: every field is a fixed sentence, a stable code or a wait, so
    /// this is already redacted and no supplied value can reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for AdmissionError {}
