//! Stable `sandbox.*` codes and the crate's error type.
//!
//! Every variant maps to one code in `crates/framework/spot-errors/catalogue/sandbox.toml`.
//! No message contains a field value, a secret, a key or a digest of a value: errors are
//! logged and stored forever (the catalogue's message rules).

use thiserror::Error;

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A sandbox failure with a stable machine-readable code.
#[derive(Debug, Error)]
pub enum Error {
    /// The plan is not usable: two names that must differ are equal, a name is empty or the
    /// snapshot belongs to another tenant. Code `sandbox.invalid_plan`.
    #[error("the sandbox plan is invalid: {reason}")]
    InvalidPlan {
        /// What is wrong with the plan, in plain words.
        reason: String,
    },

    /// The masking policy is not usable. Code `sandbox.policy`.
    #[error("the masking policy is invalid: {reason}")]
    Policy {
        /// What is wrong with the policy, in plain words.
        reason: String,
    },

    /// A workflow step was asked for in the wrong state. Code `sandbox.state`.
    #[error("step `{step}` is not allowed while the sandbox is {state}")]
    State {
        /// The step that was refused.
        step: &'static str,
        /// The state it found.
        state: String,
    },

    /// The snapshot provider failed. Code `sandbox.snapshot`.
    #[error("the snapshot step failed: {reason}")]
    Snapshot {
        /// The provider's plain-language reason (never a value or a credential).
        reason: String,
    },

    /// The restore into the sandbox tenant failed. Code `sandbox.restore`.
    #[error("the restore step failed: {reason}")]
    Restore {
        /// The provider's plain-language reason.
        reason: String,
    },

    /// The control plane refused to provision or open the tenant. Code `sandbox.tenant`.
    #[error("the tenant step failed: {reason}")]
    Tenant {
        /// The control plane's plain-language reason.
        reason: String,
    },

    /// The sandbox store could not be read or written. Code `sandbox.store`.
    #[error("the sandbox store failed: {reason}")]
    Store {
        /// The store's plain-language reason.
        reason: String,
    },

    /// Masking could not complete, including the ledger-balance guard. Code `sandbox.mask`.
    #[error("masking failed: {reason}")]
    Mask {
        /// What stopped masking, in plain words.
        reason: String,
    },

    /// The leak scan was asked for before any masking indexed source values.
    /// Code `sandbox.scan_missing`.
    #[error("no leak scan can run: masking has not indexed any source value")]
    ScanMissing,

    /// The leak scan found at least one unmasked original value, so the sandbox cannot be
    /// activated. Code `sandbox.leak`.
    #[error("the sandbox holds {leaks} unmasked value(s); it must not be activated")]
    Leak {
        /// How many leaks the report recorded.
        leaks: usize,
    },

    /// The outbound lock is missing or does not deny and discard everything.
    /// Code `sandbox.effects`.
    #[error("the outbound effects are not locked off: {reason}")]
    Effects {
        /// What is missing from the lock.
        reason: String,
    },
}

impl Error {
    /// The stable dotted code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidPlan { .. } => codes::INVALID_PLAN,
            Error::Policy { .. } => codes::POLICY,
            Error::State { .. } => codes::STATE,
            Error::Snapshot { .. } => codes::SNAPSHOT,
            Error::Restore { .. } => codes::RESTORE,
            Error::Tenant { .. } => codes::TENANT,
            Error::Store { .. } => codes::STORE,
            Error::Mask { .. } => codes::MASK,
            Error::ScanMissing => codes::SCAN_MISSING,
            Error::Leak { .. } => codes::LEAK,
            Error::Effects { .. } => codes::EFFECTS,
        }
    }
}

/// The code vocabulary, so callers match on constants rather than strings.
pub mod codes {
    /// `sandbox.invalid_plan`.
    pub const INVALID_PLAN: &str = "sandbox.invalid_plan";
    /// `sandbox.policy`.
    pub const POLICY: &str = "sandbox.policy";
    /// `sandbox.state`.
    pub const STATE: &str = "sandbox.state";
    /// `sandbox.snapshot`.
    pub const SNAPSHOT: &str = "sandbox.snapshot";
    /// `sandbox.restore`.
    pub const RESTORE: &str = "sandbox.restore";
    /// `sandbox.tenant`.
    pub const TENANT: &str = "sandbox.tenant";
    /// `sandbox.store`.
    pub const STORE: &str = "sandbox.store";
    /// `sandbox.mask`.
    pub const MASK: &str = "sandbox.mask";
    /// `sandbox.scan_missing`.
    pub const SCAN_MISSING: &str = "sandbox.scan_missing";
    /// `sandbox.leak`.
    pub const LEAK: &str = "sandbox.leak";
    /// `sandbox.effects`.
    pub const EFFECTS: &str = "sandbox.effects";
}
