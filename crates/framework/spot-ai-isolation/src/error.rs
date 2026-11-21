//! Stable error codes for AI workload isolation: the `ai_isolation.*` family.
//!
//! Callers match on [`AiError::code`], never on the message. Every code belongs to the
//! `ai_isolation.*` family and is catalogued in `spot-errors`.

use std::fmt;
use std::time::Duration;

/// The AI pool (or the effective admission budget) is full: the call was refused rather than
/// queued without a bound.
pub const BUSY: &str = "ai_isolation.busy";

/// A per-agent or per-tenant request rate is spent; the message carries when to retry.
pub const RATE_LIMITED: &str = "ai_isolation.rate_limited";

/// A call's or a session's deadline had already passed when it was admitted.
pub const DEADLINE_EXCEEDED: &str = "ai_isolation.deadline_exceeded";

/// The agent's or the tenant's circuit breaker is open; the call fails at once instead of
/// waiting on a struggling dependency.
pub const CIRCUIT_OPEN: &str = "ai_isolation.circuit_open";

/// A per-call bound (rows or bytes) was exceeded.
pub const LIMIT_EXCEEDED: &str = "ai_isolation.limit_exceeded";

/// The call was cancelled before it completed; its statement is cancelled and its permit
/// released.
pub const CANCELLED: &str = "ai_isolation.cancelled";

/// Interactive load is above the high threshold and AI admission has shrunk to zero: the call
/// is shed first so human traffic keeps the database.
pub const SHED: &str = "ai_isolation.shed";

/// An argument is outside its range: an empty or over-long identity, a zero pool size, a
/// non-positive timeout, or a pool setting that does not fit.
pub const INVALID_ARGUMENT: &str = "ai_isolation.invalid_argument";

/// The cluster-wide coordinator could not be reached to take the AI request token.
pub const COORDINATOR_UNAVAILABLE: &str = "ai_isolation.coordinator_unavailable";

/// What went wrong in one admission decision.
///
/// `code` is stable and callers match on it, not on the message. `retry_after` is set when the
/// caller may sensibly try again after that long (a spent rate limit, an open breaker, a full
/// pool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiError {
    /// The stable machine-readable code (`ai_isolation.busy`, ...).
    pub code: &'static str,
    /// What went wrong, naming no secret and no raw data.
    pub message: String,
    /// How long until a retry may succeed, when known.
    pub retry_after: Option<Duration>,
}

impl AiError {
    /// An error with the given code and message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Attaches a retry hint.
    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    /// A rate limit is spent.
    pub fn rate_limited(after: Duration) -> Self {
        Self::new(
            RATE_LIMITED,
            "the request rate for this scope is spent; retry later",
        )
        .with_retry_after(after)
    }

    /// The admission budget is full.
    pub fn busy(scope: &str) -> Self {
        Self::new(BUSY, format!("AI admission is full at the {scope} scope"))
    }

    /// A deadline had already passed.
    pub fn deadline_exceeded(what: &str) -> Self {
        Self::new(
            DEADLINE_EXCEEDED,
            format!("the {what} deadline has already passed"),
        )
    }

    /// A per-call bound was exceeded.
    pub fn limit_exceeded(what: &str) -> Self {
        Self::new(
            LIMIT_EXCEEDED,
            format!("the per-call {what} limit was exceeded"),
        )
    }

    /// An argument is invalid.
    pub fn invalid_argument(why: impl Into<String>) -> Self {
        Self::new(INVALID_ARGUMENT, why)
    }

    /// A call was cancelled by its caller before it completed.
    pub fn cancelled() -> Self {
        Self::new(CANCELLED, "the call was cancelled")
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AiError {}

/// The result of every fallible admission operation.
pub type Result<T> = std::result::Result<T, AiError>;
