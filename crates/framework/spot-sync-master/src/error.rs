//! Stable error codes for master-data propagation: the `sync_master.*` family.
//!
//! Callers match on [`Error::code`], never on the message. Every code here is library-internal
//! for now: the metadata and control-plane surfaces (WP-SYN-01, WP-SYN-02) map them onto the
//! app-facing API. They are catalogued in
//! `crates/framework/spot-errors/catalogue/sync_master.toml`.

use std::fmt;

/// A value passed to a constructor or a routing call is missing, out of range or
/// self-contradictory.
pub const INVALID_ARGUMENT: &str = "sync_master.invalid_argument";

/// Two subscriptions name the same node, so the node registry has an ambiguous entry for it.
pub const DUPLICATE_SUBSCRIPTION: &str = "sync_master.duplicate_subscription";

/// The convergence harness could not finish: a simulated branch did not converge under the
/// injected faults, or an arithmetic step overflowed.
pub const HARNESS_FAILED: &str = "sync_master.harness_failed";

/// The `sync_master.*` vocabulary this crate defines, so the error-code scan sees every code
/// exactly where it is used. Each code's literal also appears at the constructor that raises it.
pub const CODES: [&str; 3] = [INVALID_ARGUMENT, DUPLICATE_SUBSCRIPTION, HARNESS_FAILED];

/// What went wrong in one master-data propagation call.
///
/// `code` is stable and callers match on it, not on the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`sync_master.invalid_argument`, ...).
    pub code: &'static str,
    /// What went wrong. Never a secret and never a raw SQL string.
    pub message: String,
}

impl Error {
    /// An error with the given code and message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An argument is missing or out of range.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(INVALID_ARGUMENT, message)
    }

    /// Two subscriptions name the same node.
    pub fn duplicate_subscription(node: impl fmt::Display) -> Self {
        Self::new(
            DUPLICATE_SUBSCRIPTION,
            format!("two subscriptions name the same node: {node}"),
        )
    }

    /// The convergence harness could not finish.
    pub fn harness_failed(message: impl Into<String>) -> Self {
        Self::new(HARNESS_FAILED, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Result of every fallible master-data propagation operation.
pub type Result<T> = std::result::Result<T, Error>;
