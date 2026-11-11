//! Typed errors with stable [`codes`](crate::codes).

use crate::codes;

/// Anything the side-effect hold can refuse to do.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A site already has a live hold.
    #[error("site {site} already has a live hold for migration {migration_id}")]
    AlreadyHeld {
        /// The held site.
        site: String,
        /// The migration of the live hold.
        migration_id: String,
    },
    /// A transition was asked for on a site that is not held.
    #[error("site {site} is not held")]
    NotHeld {
        /// The site.
        site: String,
    },
    /// A release, discard or confirmation named the wrong migration.
    #[error("migration {requested} does not match the live hold {held} on site {site}")]
    WrongMigration {
        /// The site.
        site: String,
        /// The migration of the live hold (or `-` when none is live).
        held: String,
        /// The migration the caller named.
        requested: String,
    },
    /// The hold ledger could not be read, so the site is treated as held.
    #[error("the hold ledger for site {site} could not be read; the site is treated as held")]
    StateUnknown {
        /// The site.
        site: String,
    },
    /// A configuration tried to make fiscal transmissions holdable.
    #[error(
        "fiscal transmissions may never be held, so class {class} cannot set never_hold = false"
    )]
    FiscalCannotBeHeld {
        /// The offending class key.
        class: String,
    },
    /// A held effect is older than its class' `max_hold` and needs explicit confirmation.
    #[error("effect {key} of class {class} is older than max_hold and needs confirmation")]
    ReleaseNeedsConfirmation {
        /// The effect's idempotency key.
        key: String,
        /// The class key.
        class: String,
    },
    /// The hold ledger is unavailable.
    #[error("the hold ledger is unavailable: {reason}")]
    LedgerUnavailable {
        /// What the backend reported.
        reason: String,
    },
    /// A dispatcher failed while releasing a held effect.
    #[error("dispatching effect {key} of class {class} failed: {reason}")]
    DispatchFailed {
        /// The effect's idempotency key.
        key: String,
        /// The class key.
        class: String,
        /// What the dispatcher reported.
        reason: String,
    },
    /// A class asked to be `never_hold` without a lead-approved allow-list entry.
    #[error(
        "class {class} may not be never_hold: it is not in the lead-approved never_hold_allow list"
    )]
    NeverHoldNotAllowed {
        /// The offending class key.
        class: String,
    },
    /// An identifier or argument is not in its documented form.
    #[error("invalid {what}: {reason}")]
    InvalidInput {
        /// What was invalid (`migration id`, `effect kind`, ...).
        what: String,
        /// Why.
        reason: String,
    },
    /// Another node is already releasing this site's hold.
    #[error("another node is already releasing the hold on site {site}")]
    ReleaseInProgress {
        /// The site.
        site: String,
    },
    /// Recording a hold transition in the audit chain failed.
    #[error("recording the hold transition in the audit chain failed: {reason}")]
    AuditFailed {
        /// What the audit backend reported.
        reason: String,
    },
}

impl Error {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::AlreadyHeld { .. } => codes::ALREADY_HELD,
            Error::NotHeld { .. } => codes::NOT_HELD,
            Error::WrongMigration { .. } => codes::WRONG_MIGRATION,
            Error::StateUnknown { .. } => codes::STATE_UNKNOWN,
            Error::FiscalCannotBeHeld { .. } => codes::FISCAL_CANNOT_BE_HELD,
            Error::ReleaseNeedsConfirmation { .. } => codes::RELEASE_NEEDS_CONFIRMATION,
            Error::LedgerUnavailable { .. } => codes::LEDGER_UNAVAILABLE,
            Error::DispatchFailed { .. } => codes::DISPATCH_FAILED,
            Error::NeverHoldNotAllowed { .. } => codes::NEVER_HOLD_NOT_ALLOWED,
            Error::InvalidInput { .. } => codes::INVALID_INPUT,
            Error::ReleaseInProgress { .. } => codes::RELEASE_IN_PROGRESS,
            Error::AuditFailed { .. } => codes::AUDIT_FAILED,
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
