//! [`SignInAdmission`]: the one question `login` asks before it looks at a name or a password.
//!
//! Sign-in is admitted through a shared, restart-proof throttle (WP-IAM-11.T11, ADR-083 task 7).
//! The port has two answers that matter to the caller and one that must never be an allow:
//!
//! - [`Admission::Allowed`]: the attempt has been counted; go on to the lookup and the check;
//! - [`Admission::Denied`]: the attempt is over a limit; refuse it and say when to retry;
//! - [`AdmissionError::Unavailable`]: the counters could not be reached or read, so nothing can
//!   be counted. The caller refuses with a distinct, retryable error (`login.admission_unavailable`).
//!   There is no path from an error to an allow.
//!
//! [`CoordinatorAdmission`] is the production implementation over
//! [`CoordinatorThrottle`](spot_throttle_coordinator::CoordinatorThrottle). An unknown account name
//! is passed exactly like a real one (same call, same cost): the throttle has no map to fill or
//! evict, which closes a recorded security finding for this crate. The raw name and the source address go to the
//! throttle only as arguments; it keys its buckets by a salted digest, and nothing here logs or
//! returns either.
//!
//! [`AllowAll`] admits everything. It exists for tests only: it is compiled under `cfg(test)` or
//! the `testing` feature, and no production constructor in this crate returns it.

use std::sync::Arc;
use std::time::Duration;

use spot_throttle_coordinator::{Attempt, CoordinatorThrottle, Kind, SourceAddr, Verdict};

use crate::ports::BoxFuture;

/// One sign-in attempt, as the admission port sees it.
///
/// Nothing in it is a credential. `account` is the name **as typed** (trimmed and lower-cased by
/// the throttle), whether or not it belongs to a user.
#[derive(Clone, Copy, Debug)]
pub struct SignInAttempt<'a> {
    /// The tenant the request resolved to (never a request field).
    pub tenant: &'a str,
    /// The sign-in name as typed, known or not.
    pub account: &'a str,
    /// The peer address the edge attributes the request to (`spot-http`'s trusted-proxy rules
    /// have already been applied); `None` when it could not tell.
    pub peer: Option<&'a str>,
    /// A stable id for the connection or caller, used as the source **only** when there is no
    /// peer address. An attempt with neither is refused, never counted in one shared bucket
    ///.
    pub connection_id: Option<&'a str>,
}

/// The throttle's answer to an attempt it could count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The attempt is within every limit and has been counted.
    Allowed,
    /// The attempt is over a limit. The same attempt can succeed after `retry_after`.
    Denied {
        /// How long until the denying bucket has refilled enough.
        retry_after: Duration,
    },
}

/// Why an attempt could not be counted at all. Every variant is a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    /// The counters could not be reached, a salt could not be had, or the attempt could not be
    /// formed. `code` is the stable throttle code, for logs only; it is never sent to a client.
    Unavailable {
        /// The stable `throttle_coordinator.*` code of the cause.
        code: &'static str,
    },
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { code } => {
                write!(f, "sign-in admission is unavailable ({code}); refuse")
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

/// The admission port. `LoginService` takes one at construction; there is no constructor that
/// leaves it unset.
pub trait SignInAdmission: Send + Sync + 'static {
    /// Counts `attempt` and says whether it may go on.
    ///
    /// An implementation that cannot count returns [`AdmissionError`]; it never answers
    /// [`Admission::Allowed`] without having counted.
    fn admit<'a>(
        &'a self,
        attempt: SignInAttempt<'a>,
    ) -> BoxFuture<'a, Result<Admission, AdmissionError>>;
}

/// Admission through the coordinator throttle: shared by every node and kept across a restart.
#[derive(Clone, Debug)]
pub struct CoordinatorAdmission {
    throttle: Arc<CoordinatorThrottle>,
}

impl CoordinatorAdmission {
    /// Admission over `throttle`. The salt ring and the limits are the caller's: this crate
    /// holds no salt.
    #[must_use]
    pub fn new(throttle: Arc<CoordinatorThrottle>) -> Self {
        Self { throttle }
    }
}

impl SignInAdmission for CoordinatorAdmission {
    fn admit<'a>(
        &'a self,
        attempt: SignInAttempt<'a>,
    ) -> BoxFuture<'a, Result<Admission, AdmissionError>> {
        Box::pin(async move {
            let source = match attempt.peer.filter(|peer| !peer.trim().is_empty()) {
                Some(peer) => SourceAddr::new(peer),
                None => SourceAddr {
                    address: "",
                    connection_id: attempt.connection_id,
                },
            };
            let counted = Attempt {
                kind: Kind::SignIn,
                account: Some(attempt.account),
                source,
                destination: None,
                tenant: attempt.tenant,
            };
            match self.throttle.admit(&counted).await {
                Ok(Verdict::Allow) => Ok(Admission::Allowed),
                Ok(Verdict::Deny { retry_after }) => Ok(Admission::Denied { retry_after }),
                // Not produced by `admit`, but a denial is a denial wherever it comes from.
                Err(spot_throttle_coordinator::ThrottleError::RateLimited { retry_after }) => {
                    Ok(Admission::Denied { retry_after })
                }
                // Coordinator down, salt missing, malformed attempt: none of them may admit.
                Err(error) => Err(AdmissionError::Unavailable { code: error.code() }),
            }
        })
    }
}

/// Admits every attempt without counting. **For tests only.**
#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

#[cfg(any(test, feature = "testing"))]
impl SignInAdmission for AllowAll {
    fn admit<'a>(
        &'a self,
        _attempt: SignInAttempt<'a>,
    ) -> BoxFuture<'a, Result<Admission, AdmissionError>> {
        Box::pin(async { Ok(Admission::Allowed) })
    }
}
