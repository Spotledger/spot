//! Typed errors with stable `migration_audit.*` codes.
//!
//! Every failure the crate can return names a code a console or an operator can look up. A
//! message never carries a payload value, a plan secret or a connection string.

/// Stable codes of this crate (`migration_audit.*`, catalogued in `migration_audit.toml`).
pub mod codes {
    /// A run id is empty, too long or holds a character a run id may not.
    pub const BAD_ID: &str = "migration_audit.bad_id";
    /// An actor is not `system`, `user:<id>` or `agent:<id>`.
    pub const BAD_ACTOR: &str = "migration_audit.bad_actor";
    /// A run with this id has already begun on this recorder.
    pub const DUPLICATE_RUN: &str = "migration_audit.duplicate_run";
    /// The principal does not hold the `migration.run` permission.
    pub const NOT_PERMITTED: &str = "migration_audit.not_permitted";
    /// A run handle was presented for another site.
    pub const WRONG_SITE: &str = "migration_audit.wrong_site";
    /// An outcome is not one a run can finish on.
    pub const BAD_OUTCOME: &str = "migration_audit.bad_outcome";
    /// A stored lifecycle event is malformed.
    pub const RECORD_INVALID: &str = "migration_audit.record_invalid";
    /// The audit chain store or signer failed.
    pub const CHAIN: &str = "migration_audit.chain";
    /// The chain failed verification: an entry was altered, removed or reordered.
    pub const CHAIN_BROKEN: &str = "migration_audit.chain_broken";
}

/// Anything this crate refuses to do, or cannot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MigrationAuditError {
    /// A run id is empty, longer than 256 bytes or holds a character outside
    /// `A-Z a-z 0-9 . _ : -`.
    #[error("{}: {id:?} is not a usable run id", codes::BAD_ID)]
    BadId {
        /// The offending text.
        id: String,
    },
    /// An actor is not `system`, `user:<id>` or `agent:<id>`.
    #[error("{}: {message}", codes::BAD_ACTOR)]
    BadActor {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The same run id was begun twice on this recorder.
    #[error("{}: run {id} has already begun", codes::DUPLICATE_RUN)]
    DuplicateRun {
        /// The run id.
        id: String,
    },
    /// The acting principal does not hold the `migration.run` permission.
    #[error(
        "{}: the acting principal may not hold the migration.run permission",
        codes::NOT_PERMITTED
    )]
    NotPermitted,
    /// A run handle belongs to another site.
    #[error(
        "{}: the run belongs to site {run_site}, not {site}",
        codes::WRONG_SITE
    )]
    WrongSite {
        /// The site the handle was issued for.
        run_site: String,
        /// The site the caller presented it for.
        site: String,
    },
    /// An outcome that is not terminal was passed to
    /// [`finish`](crate::RunRecorder::finish).
    #[error(
        "{}: {outcome} is not an outcome a run can finish on",
        codes::BAD_OUTCOME
    )]
    BadOutcome {
        /// The offending outcome.
        outcome: String,
    },
    /// A stored lifecycle event is malformed: a field is missing or the wrong type.
    #[error("{}: {message}", codes::RECORD_INVALID)]
    RecordInvalid {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The audit chain store or its signer failed.
    #[error("{}: {message}", codes::CHAIN)]
    Chain {
        /// What the backend reported (never a payload value).
        message: String,
    },
    /// The chain failed verification.
    #[error("{}: {message}", codes::CHAIN_BROKEN)]
    ChainBroken {
        /// What was found, in a sentence.
        message: String,
    },
}

impl MigrationAuditError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadId { .. } => codes::BAD_ID,
            Self::BadActor { .. } => codes::BAD_ACTOR,
            Self::DuplicateRun { .. } => codes::DUPLICATE_RUN,
            Self::NotPermitted => codes::NOT_PERMITTED,
            Self::WrongSite { .. } => codes::WRONG_SITE,
            Self::BadOutcome { .. } => codes::BAD_OUTCOME,
            Self::RecordInvalid { .. } => codes::RECORD_INVALID,
            Self::Chain { .. } => codes::CHAIN,
            Self::ChainBroken { .. } => codes::CHAIN_BROKEN,
        }
    }
}

impl From<spot_audit_chain::AuditError> for MigrationAuditError {
    fn from(error: spot_audit_chain::AuditError) -> Self {
        Self::Chain {
            message: error.to_string(),
        }
    }
}
